//! Deterministic code-structure extraction with tree-sitter: symbols (functions, classes,
//! types…), and edges (imports, calls, inheritance). No LLM is involved.

use std::path::Path;
use tree_sitter::{Node, Parser};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    Go,
    JavaScript,
    TypeScript,
    Tsx,
    Swift,
    Kotlin,
}

impl Lang {
    pub fn from_path(p: &Path) -> Option<Lang> {
        let ext = p.extension()?.to_string_lossy().to_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "swift" => Lang::Swift,
            "kt" | "kts" => Lang::Kotlin,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Swift => "swift",
            Lang::Kotlin => "kotlin",
        }
    }

    fn grammar(&self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        }
    }
}

/// Language name from a file name, also for languages without an AST grammar.
pub fn lang_label(p: &Path) -> Option<String> {
    if let Some(l) = Lang::from_path(p) {
        return Some(l.name().to_string());
    }
    p.extension().map(|e| e.to_string_lossy().to_lowercase())
}

#[derive(Clone, Debug)]
pub struct Sym {
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub start: usize,
    pub end: usize,
    pub signature: String,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub kind: &'static str,
    pub source: Option<String>,
    pub target: String,
    pub line: usize,
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub symbols: Vec<Sym>,
    pub edges: Vec<Edge>,
}

pub fn parse(lang: Lang, src: &str) -> Parsed {
    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return Parsed::default();
    }
    let Some(tree) = parser.parse(src, None) else {
        return Parsed::default();
    };
    let mut ex = Extractor {
        lang,
        src,
        out: Parsed::default(),
    };
    let mut scope = Vec::new();
    ex.walk(tree.root_node(), &mut scope, 0);
    ex.out
}

struct Extractor<'a> {
    lang: Lang,
    src: &'a str,
    out: Parsed,
}

/// A definition found at a node: (symbol kind, name, opens a scope for children).
struct Def {
    kind: String,
    name: String,
    /// `impl`/`extension` blocks open a scope without being a symbol themselves.
    is_symbol: bool,
}

impl<'a> Extractor<'a> {
    fn text(&self, n: Node) -> &'a str {
        &self.src[n.byte_range()]
    }

    fn field_text(&self, n: Node, field: &str) -> Option<String> {
        n.child_by_field_name(field)
            .map(|c| self.text(c).trim().to_string())
    }

    fn first_ident(&self, n: Node) -> Option<String> {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            if ch.kind().contains("identifier") {
                return Some(self.text(ch).to_string());
            }
        }
        None
    }

    fn first_token_kind(&self, n: Node) -> String {
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            if !ch.is_named() {
                return ch.kind().to_string();
            }
            if ch.kind() == "modifiers" {
                let t = self.text(ch);
                if t.contains("enum") {
                    return "enum".into();
                }
                continue;
            }
        }
        String::new()
    }

    fn in_type_scope(scope: &[(String, bool)]) -> bool {
        scope.last().map(|(_, t)| *t).unwrap_or(false)
    }

    fn def(&self, n: Node, scope: &[(String, bool)]) -> Option<Def> {
        let k = n.kind();
        let method_or_fn = || {
            if Self::in_type_scope(scope) {
                "method".to_string()
            } else {
                "function".to_string()
            }
        };
        let named = |kind: &str| -> Option<Def> {
            let name = self.field_text(n, "name").or_else(|| self.first_ident(n))?;
            Some(Def {
                kind: kind.to_string(),
                name,
                is_symbol: true,
            })
        };
        match self.lang {
            Lang::Rust => match k {
                "function_item" | "function_signature_item" => named(&method_or_fn()),
                "struct_item" => named("struct"),
                "enum_item" => named("enum"),
                "union_item" => named("union"),
                "trait_item" => named("trait"),
                "type_item" => named("type"),
                "mod_item" => named("module"),
                "const_item" => named("const"),
                "static_item" => named("static"),
                "macro_definition" => named("macro"),
                "impl_item" => {
                    let ty = self.field_text(n, "type")?;
                    let ty = ty.split('<').next().unwrap_or(&ty).trim().to_string();
                    Some(Def {
                        kind: "impl".into(),
                        name: ty,
                        is_symbol: false,
                    })
                }
                _ => None,
            },
            Lang::Python => match k {
                "function_definition" => named(&method_or_fn()),
                "class_definition" => named("class"),
                _ => None,
            },
            Lang::Go => match k {
                "function_declaration" => named("function"),
                "method_declaration" => named("method"),
                "type_spec" | "type_alias" => {
                    let kind = match n.child_by_field_name("type").map(|t| t.kind()) {
                        Some("struct_type") => "struct",
                        Some("interface_type") => "interface",
                        _ => "type",
                    };
                    named(kind)
                }
                "const_spec" if scope.is_empty() => named("const"),
                _ => None,
            },
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match k {
                "function_declaration" | "generator_function_declaration" => named(&method_or_fn()),
                "class_declaration" | "abstract_class_declaration" | "class" => named("class"),
                "method_definition" => named("method"),
                "interface_declaration" => named("interface"),
                "type_alias_declaration" => named("type"),
                "enum_declaration" => named("enum"),
                "variable_declarator" => {
                    let v = n.child_by_field_name("value")?;
                    if matches!(
                        v.kind(),
                        "arrow_function"
                            | "function_expression"
                            | "function"
                            | "generator_function"
                    ) {
                        named(&method_or_fn())
                    } else {
                        None
                    }
                }
                _ => None,
            },
            Lang::Swift => match k {
                "function_declaration" | "protocol_function_declaration" => named(&method_or_fn()),
                "init_declaration" => Some(Def {
                    kind: "method".into(),
                    name: "init".into(),
                    is_symbol: true,
                }),
                "protocol_declaration" => named("protocol"),
                "typealias_declaration" => named("type"),
                "class_declaration" => {
                    let kw = self.first_token_kind(n);
                    if kw == "extension" {
                        let name = self.field_text(n, "name")?;
                        return Some(Def {
                            kind: "extension".into(),
                            name,
                            is_symbol: false,
                        });
                    }
                    let kind = match kw.as_str() {
                        "struct" => "struct",
                        "enum" => "enum",
                        "actor" => "actor",
                        _ => "class",
                    };
                    named(kind)
                }
                _ => None,
            },
            Lang::Kotlin => match k {
                "function_declaration" => named(&method_or_fn()),
                "class_declaration" => {
                    let t = self.text(n);
                    let head = t.split('{').next().unwrap_or("");
                    let kind = if head.contains("interface ") {
                        "interface"
                    } else if head.contains("enum ") {
                        "enum"
                    } else {
                        "class"
                    };
                    named(kind)
                }
                "object_declaration" => named("object"),
                "type_alias" => {
                    let name = self.field_text(n, "type").or_else(|| self.first_ident(n))?;
                    Some(Def {
                        kind: "type".into(),
                        name,
                        is_symbol: true,
                    })
                }
                _ => None,
            },
        }
    }

    fn signature(&self, n: Node) -> String {
        let t = self.text(n);
        let mut sig = String::new();
        for line in t.lines().take(4) {
            let l = line.trim();
            sig.push_str(l);
            if l.contains('{') || l.ends_with(':') || l.ends_with(';') || l.contains("=>") {
                break;
            }
            sig.push(' ');
        }
        let sig = sig.split('{').next().unwrap_or(&sig).trim();
        let sig = sig.trim_end_matches(':').trim();
        crate::util::clip(sig, 200)
    }

    fn doc(&self, n: Node) -> Option<String> {
        // Leading comments directly above the definition.
        let mut lines = Vec::new();
        let mut prev = n.prev_sibling();
        let mut expected_row = n.start_position().row;
        while let Some(p) = prev {
            if !p.kind().contains("comment") || p.end_position().row + 1 < expected_row {
                break;
            }
            lines.push(self.text(p).to_string());
            expected_row = p.start_position().row;
            prev = p.prev_sibling();
        }
        lines.reverse();
        let mut doc = lines
            .iter()
            .map(|l| {
                l.trim()
                    .trim_start_matches("///")
                    .trim_start_matches("//!")
                    .trim_start_matches("//")
                    .trim_start_matches("/**")
                    .trim_start_matches("/*")
                    .trim_end_matches("*/")
                    .trim_start_matches('*')
                    .trim_start_matches('#')
                    .trim()
                    .to_string()
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        // Python docstrings.
        if doc.is_empty() && self.lang == Lang::Python {
            if let Some(body) = n.child_by_field_name("body") {
                if let Some(first) = body.named_child(0) {
                    if first.kind() == "expression_statement" {
                        if let Some(s) = first.named_child(0) {
                            if s.kind() == "string" {
                                doc = self
                                    .text(s)
                                    .trim_matches(|c| c == '"' || c == '\'')
                                    .trim()
                                    .to_string();
                            }
                        }
                    }
                }
            }
        }
        if doc.is_empty() {
            None
        } else {
            Some(crate::util::clip(
                &doc.split_whitespace().collect::<Vec<_>>().join(" "),
                300,
            ))
        }
    }

    fn callee(&self, n: Node) -> Option<String> {
        let k = n.kind();
        let f = match (self.lang, k) {
            (Lang::Swift, "call_expression") | (Lang::Kotlin, "call_expression") => {
                n.named_child(0)?
            }
            (_, "new_expression") => n.child_by_field_name("constructor")?,
            _ => n.child_by_field_name("function")?,
        };
        self.last_name(f)
    }

    /// The trailing identifier of an expression like `a.b::c` or `self.other.go`.
    fn last_name(&self, n: Node) -> Option<String> {
        match n.kind() {
            "identifier"
            | "simple_identifier"
            | "field_identifier"
            | "property_identifier"
            | "type_identifier" => Some(self.text(n).to_string()),
            "field_expression" => n
                .child_by_field_name("field")
                .and_then(|f| self.last_name(f)),
            "scoped_identifier" => n
                .child_by_field_name("name")
                .and_then(|f| self.last_name(f)),
            "attribute" => n
                .child_by_field_name("attribute")
                .and_then(|f| self.last_name(f)),
            "selector_expression" => n
                .child_by_field_name("field")
                .and_then(|f| self.last_name(f)),
            "member_expression" => n
                .child_by_field_name("property")
                .and_then(|f| self.last_name(f)),
            "generic_function" => n
                .child_by_field_name("function")
                .and_then(|f| self.last_name(f)),
            "navigation_expression" => {
                if let Some(s) = n.child_by_field_name("suffix") {
                    return self.last_name(s);
                }
                let c = n.named_child(n.named_child_count().checked_sub(1)?)?;
                self.last_name(c)
            }
            "navigation_suffix" => n
                .child_by_field_name("suffix")
                .and_then(|f| self.last_name(f)),
            _ => {
                let t = self.text(n);
                let last = t
                    .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .find(|s| !s.is_empty())?;
                if last.chars().next()?.is_alphabetic() || last.starts_with('_') {
                    Some(last.to_string())
                } else {
                    None
                }
            }
        }
    }

    fn imports(&self, n: Node) -> Vec<String> {
        let unquote = |s: &str| {
            s.trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_string()
        };
        match (self.lang, n.kind()) {
            (Lang::Rust, "use_declaration") => n
                .child_by_field_name("argument")
                .map(|a| vec![self.text(a).to_string()])
                .unwrap_or_default(),
            (Lang::Python, "import_statement") => {
                let mut v = Vec::new();
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    let t = match ch.kind() {
                        "aliased_import" => ch.child_by_field_name("name").map(|x| self.text(x)),
                        "dotted_name" => Some(self.text(ch)),
                        _ => None,
                    };
                    if let Some(t) = t {
                        v.push(t.to_string());
                    }
                }
                v
            }
            (Lang::Python, "import_from_statement") => n
                .child_by_field_name("module_name")
                .map(|m| vec![self.text(m).to_string()])
                .unwrap_or_default(),
            (Lang::Go, "import_spec") => n
                .child_by_field_name("path")
                .map(|p| vec![unquote(self.text(p))])
                .unwrap_or_default(),
            (
                Lang::JavaScript | Lang::TypeScript | Lang::Tsx,
                "import_statement" | "export_statement",
            ) => n
                .child_by_field_name("source")
                .map(|s| vec![unquote(self.text(s))])
                .unwrap_or_default(),
            (Lang::Swift, "import_declaration") => self
                .first_ident(n)
                .or_else(|| n.named_child(0).map(|c| self.text(c).to_string()))
                .map(|s| vec![s])
                .unwrap_or_default(),
            (Lang::Kotlin, "import") => n
                .named_child(0)
                .map(|c| vec![self.text(c).to_string()])
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    fn inherits(&self, n: Node, def: &Def) -> Vec<String> {
        let mut out = Vec::new();
        match (self.lang, n.kind()) {
            (Lang::Rust, "impl_item") => {
                if let Some(t) = self.field_text(n, "trait") {
                    out.push(t.split('<').next().unwrap_or(&t).trim().to_string());
                }
            }
            (Lang::Python, "class_definition") => {
                if let Some(sc) = n.child_by_field_name("superclasses") {
                    let mut c = sc.walk();
                    for ch in sc.named_children(&mut c) {
                        if let Some(nm) = self.last_name(ch) {
                            out.push(nm);
                        }
                    }
                }
            }
            (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, _)
                if def.kind == "class" || def.kind == "interface" =>
            {
                self.collect_kinds(n, &["class_heritage", "extends_type_clause"], &mut |h| {
                    let mut c = h.walk();
                    for cl in h.named_children(&mut c) {
                        let mut c2 = cl.walk();
                        for t in cl.named_children(&mut c2) {
                            if let Some(nm) = self.last_name(t) {
                                out.push(nm);
                            }
                        }
                        if cl.kind() == "identifier" || cl.kind() == "type_identifier" {
                            out.push(self.text(cl).to_string());
                        }
                    }
                });
            }
            (Lang::Swift, _) => {
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() == "inheritance_specifier" {
                        if let Some(t) = ch.child_by_field_name("inherits_from") {
                            out.push(
                                self.text(t)
                                    .split('<')
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            (Lang::Kotlin, _) => {
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() == "delegation_specifiers" {
                        let mut c2 = ch.walk();
                        for d in ch.named_children(&mut c2) {
                            let t = self.text(d);
                            let name = t.split(['(', '<', ' ']).next().unwrap_or("").trim();
                            if !name.is_empty() {
                                out.push(name.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        out.retain(|s| !s.is_empty());
        out
    }

    fn collect_kinds(&self, n: Node, kinds: &[&str], f: &mut dyn FnMut(Node)) {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            if kinds.contains(&ch.kind()) {
                f(ch);
            } else if ch.kind() != "class_body" && ch.kind() != "statement_block" {
                self.collect_kinds(ch, kinds, f);
            }
        }
    }

    fn is_call(&self, k: &str) -> bool {
        matches!(
            (self.lang, k),
            (Lang::Rust, "call_expression")
                | (Lang::Python, "call")
                | (Lang::Go, "call_expression")
                | (
                    Lang::JavaScript | Lang::TypeScript | Lang::Tsx,
                    "call_expression" | "new_expression"
                )
                | (Lang::Swift, "call_expression")
                | (Lang::Kotlin, "call_expression")
        )
    }

    fn walk(&mut self, n: Node, scope: &mut Vec<(String, bool)>, depth: usize) {
        if depth > 200 {
            return;
        }
        let line = n.start_position().row + 1;
        let current = || scope.last().map(|(s, _)| s.clone());

        // Imports.
        for imp in self.imports(n) {
            if !imp.is_empty() {
                self.out.edges.push(Edge {
                    kind: "import",
                    source: None,
                    target: imp,
                    line,
                });
            }
        }

        // Calls (and CommonJS `require`).
        if self.is_call(n.kind()) {
            if let Some(name) = self.callee(n) {
                if name == "require" || name == "import" {
                    if let Some(arg) = n
                        .child_by_field_name("arguments")
                        .and_then(|a| a.named_child(0))
                    {
                        let t = self
                            .text(arg)
                            .trim_matches(|c| c == '"' || c == '\'' || c == '`');
                        self.out.edges.push(Edge {
                            kind: "import",
                            source: None,
                            target: t.to_string(),
                            line,
                        });
                    }
                } else {
                    self.out.edges.push(Edge {
                        kind: "call",
                        source: current(),
                        target: name,
                        line,
                    });
                }
            }
        }

        let def = self.def(n, scope);
        let mut pushed = false;
        if let Some(d) = def {
            // Go methods: parent is the receiver type.
            let mut parent = current();
            if self.lang == Lang::Go && n.kind() == "method_declaration" {
                if let Some(r) = n.child_by_field_name("receiver") {
                    let t = self.text(r);
                    if let Some(ty) = t
                        .trim_matches(|c| c == '(' || c == ')')
                        .split_whitespace()
                        .last()
                    {
                        parent = Some(
                            ty.trim_start_matches('*')
                                .split('[')
                                .next()
                                .unwrap_or(ty)
                                .to_string(),
                        );
                    }
                }
            }
            for base in self.inherits(n, &d) {
                self.out.edges.push(Edge {
                    kind: "inherit",
                    source: Some(d.name.clone()),
                    target: base,
                    line,
                });
            }
            if d.is_symbol {
                self.out.symbols.push(Sym {
                    name: d.name.clone(),
                    kind: d.kind.clone(),
                    parent: parent.clone(),
                    start: line,
                    end: n.end_position().row + 1,
                    signature: self.signature(n),
                    doc: self.doc(n),
                });
            }
            let is_type = !matches!(
                d.kind.as_str(),
                "function" | "method" | "const" | "static" | "macro" | "type"
            );
            scope.push((d.name.clone(), is_type));
            pushed = true;
        }

        let mut c = n.walk();
        let children: Vec<Node> = n.children(&mut c).collect();
        for ch in children {
            self.walk(ch, scope, depth + 1);
        }
        if pushed {
            scope.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(p: &Parsed) -> Vec<String> {
        p.symbols
            .iter()
            .map(|s| format!("{}:{}", s.kind, s.name))
            .collect()
    }

    #[test]
    fn rust_symbols_and_edges() {
        let src = "use crate::store::Store;\n/// Does things.\npub fn run(x: i32) -> i32 { helper(x); s.go() }\nstruct A;\nimpl Display for A { fn fmt(&self) {} }\n";
        let p = parse(Lang::Rust, src);
        let n = names(&p);
        assert!(n.contains(&"function:run".to_string()), "{:?}", n);
        assert!(n.contains(&"struct:A".to_string()));
        assert!(n.contains(&"method:fmt".to_string()));
        let run = p.symbols.iter().find(|s| s.name == "run").unwrap();
        assert_eq!(run.doc.as_deref(), Some("Does things."));
        assert!(p.edges.iter().any(|e| e.kind == "call"
            && e.target == "helper"
            && e.source.as_deref() == Some("run")));
        assert!(p.edges.iter().any(|e| e.kind == "call" && e.target == "go"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "import" && e.target == "crate::store::Store"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "inherit" && e.target == "Display"));
    }

    #[test]
    fn python_symbols() {
        let src = "import os\nfrom a.b import c\nclass Foo(Base):\n    \"\"\"A foo.\"\"\"\n    def bar(self):\n        return baz()\n";
        let p = parse(Lang::Python, src);
        let n = names(&p);
        assert!(n.contains(&"class:Foo".to_string()));
        assert!(n.contains(&"method:bar".to_string()));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "import" && e.target == "a.b"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "inherit" && e.target == "Base"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "call" && e.target == "baz" && e.source.as_deref() == Some("bar")));
        assert_eq!(p.symbols[0].doc.as_deref(), Some("A foo."));
    }

    #[test]
    fn typescript_symbols() {
        let src = "import { a } from './x';\nexport interface I { a(): void }\nexport class C extends B implements I { m() { foo(); this.bar(); new D(); } }\nexport const f = (x: number) => x;\n";
        let p = parse(Lang::TypeScript, src);
        let n = names(&p);
        assert!(n.contains(&"interface:I".to_string()), "{:?}", n);
        assert!(n.contains(&"class:C".to_string()));
        assert!(n.contains(&"method:m".to_string()));
        assert!(n.contains(&"function:f".to_string()));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "import" && e.target == "./x"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "inherit" && e.target == "B"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "call" && e.target == "bar"));
        assert!(p.edges.iter().any(|e| e.kind == "call" && e.target == "D"));
    }

    #[test]
    fn go_symbols() {
        let src = "package m\nimport \"fmt\"\ntype S struct{}\nfunc (s *S) M() { fmt.Println(1); helper() }\nfunc helper() {}\n";
        let p = parse(Lang::Go, src);
        let n = names(&p);
        assert!(n.contains(&"struct:S".to_string()));
        assert!(n.contains(&"method:M".to_string()));
        let m = p.symbols.iter().find(|s| s.name == "M").unwrap();
        assert_eq!(m.parent.as_deref(), Some("S"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "import" && e.target == "fmt"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "call" && e.target == "Println"));
    }

    #[test]
    fn swift_and_kotlin_symbols() {
        let sw = "import Foundation\nclass Foo: Bar {\n  func run() { helper() }\n}\nstruct S {}\n";
        let p = parse(Lang::Swift, sw);
        let n = names(&p);
        assert!(n.contains(&"class:Foo".to_string()), "{:?}", n);
        assert!(n.contains(&"struct:S".to_string()));
        assert!(n.contains(&"method:run".to_string()));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "inherit" && e.target == "Bar"));

        let kt = "import kotlin.math.max\nclass Foo(val x: Int) : Bar() {\n  fun run(y: Int): Int { return helper(y) }\n}\nfun top() {}\n";
        let p = parse(Lang::Kotlin, kt);
        let n = names(&p);
        assert!(n.contains(&"class:Foo".to_string()), "{:?}", n);
        assert!(n.contains(&"method:run".to_string()));
        assert!(n.contains(&"function:top".to_string()));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "import" && e.target == "kotlin.math.max"));
        assert!(p
            .edges
            .iter()
            .any(|e| e.kind == "call" && e.target == "helper"));
    }
}
