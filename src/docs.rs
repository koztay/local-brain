//! Local document reading: PDFs (text layer extracted locally) and images (local VLM).
//! Results are cached by content hash, so each document is read once.

use crate::config::Config;
use crate::learn::Progress;
use crate::llm::Llm;
use crate::paths::Project;
use crate::store::Store;
use crate::util;
use anyhow::{bail, Context, Result};

pub fn is_image(path: &str) -> bool {
    let p = path.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
        .iter()
        .any(|e| p.ends_with(e))
}

pub fn is_pdf(path: &str) -> bool {
    path.to_lowercase().ends_with(".pdf")
}

pub fn pdf_text(bytes: &[u8]) -> Result<String> {
    let b = bytes.to_vec();
    // pdf-extract can panic on malformed files; contain it.
    let res = std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem(&b));
    match res {
        Ok(Ok(t)) => {
            let cleaned: Vec<&str> = t.lines().map(|l| l.trim_end()).collect();
            let mut out = String::new();
            let mut blank = 0;
            for l in cleaned {
                if l.trim().is_empty() {
                    blank += 1;
                    if blank > 1 {
                        continue;
                    }
                } else {
                    blank = 0;
                }
                out.push_str(l);
                out.push('\n');
            }
            Ok(out.trim().to_string())
        }
        Ok(Err(e)) => bail!("could not extract PDF text: {}", e),
        Err(_) => bail!("could not extract PDF text (malformed PDF)"),
    }
}

pub fn pdf_pages(bytes: &[u8]) -> usize {
    let needle = b"/Type /Page";
    let alt = b"/Type/Page";
    let count = |n: &[u8]| bytes.windows(n.len()).filter(|w| *w == n).count();
    // "/Type /Pages" also matches; subtract roughly one parent node.
    (count(needle) + count(alt)).saturating_sub(1).max(1)
}

/// Image dimensions for PNG/JPEG/GIF (used to estimate what Claude would have paid).
pub fn image_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() > 24 && b.starts_with(&[0x89, b'P', b'N', b'G']) {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some((w, h));
    }
    if b.len() > 10 && b.starts_with(b"GIF8") {
        return Some((
            u16::from_le_bytes([b[6], b[7]]) as u32,
            u16::from_le_bytes([b[8], b[9]]) as u32,
        ));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
            {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// Tokens Claude would spend reading this document directly.
pub fn baseline_tokens(path: &str, bytes: &[u8], text_tokens: i64) -> i64 {
    if is_image(path) {
        // Claude scales images to ≤ ~1.15 MP; tokens ≈ w·h / 750.
        let (w, h) = image_dims(bytes).unwrap_or((1200, 900));
        let scale = ((1_150_000.0 / (w as f64 * h as f64)).sqrt()).min(1.0);
        (((w as f64 * scale) * (h as f64 * scale)) / 750.0)
            .round()
            .max(200.0) as i64
    } else {
        // PDFs are sent as text plus an image per page.
        text_tokens + pdf_pages(bytes) as i64 * 1500
    }
}

const IMAGE_PROMPT: &str = "Describe this image for a software engineer who cannot see it. \
Transcribe all visible text verbatim. If it is a UI screenshot or mockup, list screens, components, \
labels, values and states. If it is a diagram, list the nodes and the connections between them. \
If it shows an error or a stack trace, quote it exactly. Be complete but concise; no preamble.";

pub struct DocResult {
    pub text: String,
    pub baseline_tokens: i64,
}

fn extract(llm: &Llm, rel: &str, bytes: &[u8]) -> Result<(String, String)> {
    if is_pdf(rel) {
        let t = pdf_text(bytes)?;
        if t.trim().is_empty() {
            bail!("PDF has no text layer (scanned document); convert pages to images to read them with the local vision model");
        }
        Ok((t, "pdf-text".into()))
    } else if is_image(rel) {
        if !llm.has_vision() {
            bail!("no local vision model is available (set models.general or models.vision and start your runtime)");
        }
        let t = llm.vision(IMAGE_PROMPT, bytes, 900)?;
        Ok((t, llm.vision_model.clone().unwrap_or_default()))
    } else {
        bail!("{} is not an image or a PDF", rel)
    }
}

/// Read a document locally, optionally answering a question about it.
pub fn read_doc(
    store: &Store,
    project: &Project,
    cfg: &Config,
    llm: &Llm,
    path: &str,
    question: Option<&str>,
) -> Result<DocResult> {
    let rel = project.rel(std::path::Path::new(path));
    if crate::scan::Filter::new(&[]).is_secret(&rel) {
        bail!(
            "{} looks like a secret file; local-brain will not read it",
            rel
        );
    }
    let abs = project.abs(&rel);
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {}", rel))?;
    let hash = util::sha256_hex(&bytes);
    let (content, model) = match store.doc(&rel)? {
        Some((h, t, m)) if h == hash => (t, m),
        _ => {
            let (t, m) = extract(llm, &rel, &bytes)?;
            let t = crate::secrets::mask(&t);
            store.put_doc(
                &rel,
                &hash,
                if is_pdf(&rel) { "pdf" } else { "image" },
                &t,
                &m,
            )?;
            (t, m)
        }
    };
    let baseline = baseline_tokens(&rel, &bytes, util::tokens(&content));
    let q = question.map(str::trim).filter(|q| !q.is_empty());
    let text = match q {
        Some(q) if is_image(&rel) && llm.has_vision() => {
            let prompt = format!(
                "{}\n\nAnswer this question about the image precisely: {}",
                IMAGE_PROMPT, q
            );
            llm.vision(&prompt, &bytes, 600)?
        }
        Some(q) => answer_from_text(llm, &content, q)?,
        None => {
            let (t, cut) = util::clip_tokens(&content, 3500);
            if cut {
                let id = crate::raw::save(Some(project), &content, cfg.compress.raw_ttl_days)?;
                format!("{}\n[… document continues ({} tokens total). Ask brain_doc with a `question`, or brain_raw raw_id=\"{}\" with grep/range.]", t, util::tokens(&content), id)
            } else {
                t
            }
        }
    };
    Ok(DocResult {
        text: format!(
            "{} ({}, read locally by {})\n{}",
            rel,
            if is_pdf(&rel) { "PDF" } else { "image" },
            model,
            text
        ),
        baseline_tokens: baseline,
    })
}

/// Pick the paragraphs most relevant to `q`; let the local LLM answer when available.
fn answer_from_text(llm: &Llm, content: &str, q: &str) -> Result<String> {
    let paras: Vec<&str> = content
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();
    let qv = crate::embed::hash_embed(q);
    let mut scored: Vec<(f32, usize)> = paras
        .iter()
        .enumerate()
        .map(|(i, p)| (crate::embed::cosine(&qv, &crate::embed::hash_embed(p)), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut picked: Vec<usize> = Vec::new();
    let mut size = 0;
    for (_, i) in scored {
        if size > 9000 {
            break;
        }
        size += paras[i].len();
        picked.push(i);
    }
    picked.sort();
    let excerpt = picked
        .iter()
        .map(|i| paras[*i])
        .collect::<Vec<_>>()
        .join("\n\n");
    if llm.has_text() {
        let prompt = format!(
            "Answer the question using only this document excerpt. Quote exact requirements, numbers and names. If the excerpt does not contain the answer, say so.\n\nQuestion: {}\n\nExcerpt:\n{}",
            q, excerpt
        );
        if let Ok(a) = llm.text(&prompt, 500) {
            return Ok(format!(
                "Answer (local model): {}\n\nRelevant excerpt:\n{}",
                a.trim(),
                util::clip_tokens(&excerpt, 1200).0
            ));
        }
    }
    Ok(format!(
        "Most relevant passages for \"{}\":\n{}",
        q,
        util::clip_tokens(&excerpt, 2500).0
    ))
}

/// The learn stage for documents: extract text for new/changed PDFs and images.
pub fn learn_docs(
    store: &Store,
    project: &Project,
    cfg: &Config,
    llm: &Llm,
    no_llm: bool,
    quiet: bool,
    warnings: &mut Vec<String>,
) -> Result<usize> {
    let docs: Vec<_> = store
        .files()?
        .into_iter()
        .filter(|f| f.kind == "doc")
        .collect();
    let vision = !no_llm && llm.has_vision();
    let todo: Vec<_> = docs
        .into_iter()
        .filter(|f| !matches!(store.doc(&f.path), Ok(Some((ref h, _, _))) if *h == f.hash))
        .filter(|f| is_pdf(&f.path) || vision)
        .collect();
    let mut images = 0;
    let mut prog = Progress::new("documents", todo.len(), !quiet);
    let mut done = 0;
    for f in todo {
        prog.inc();
        if is_image(&f.path) {
            if images >= cfg.learn.max_images {
                continue;
            }
            images += 1;
        }
        let bytes = match std::fs::read(project.abs(&f.path)) {
            Ok(b) => b,
            Err(_) => continue,
        };
        match extract(llm, &f.path, &bytes) {
            Ok((text, model)) => {
                let text = crate::secrets::mask(&text);
                store.put_doc(
                    &f.path,
                    &f.hash,
                    if is_pdf(&f.path) { "pdf" } else { "image" },
                    &text,
                    &model,
                )?;
                let tx = store.conn.unchecked_transaction()?;
                store.clear_file_index(f.id)?;
                let lines: Vec<&str> = text.lines().collect();
                let mut start = 0;
                while start < lines.len() {
                    let end = (start + 50).min(lines.len());
                    let body = lines[start..end].join("\n");
                    if !body.trim().is_empty() {
                        store.insert_chunk(
                            f.id,
                            &f.path,
                            Some("document"),
                            start as i64 + 1,
                            end as i64,
                            &body,
                        )?;
                    }
                    start = end;
                }
                store.put_summary(
                    "file",
                    &f.path,
                    &util::clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), 300),
                    &f.hash,
                    false,
                    &model,
                )?;
                store.set_stage(f.id, 3)?;
                tx.commit()?;
                done += 1;
            }
            Err(e) => warnings.push(format!("{}: {}", f.path, e)),
        }
    }
    prog.finish();
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dims() {
        let mut b = vec![
            0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        b.extend_from_slice(&800u32.to_be_bytes());
        b.extend_from_slice(&600u32.to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        assert_eq!(image_dims(&b), Some((800, 600)));
        assert!(baseline_tokens("a.png", &b, 0) > 500);
    }
}
