//! `(use "name")`: load `name.scm` from the server's Scheme library directory.
//!
//! The directory is chosen by whoever starts the server (`--scheme-library`), read
//! only, and every file still passes the sandbox's source checks. A `use` form is
//! expanded in place before compiling: the file's own top-level redefinitions are
//! rewritten first (so re-`use` after editing a toolkit updates existing callers),
//! macro definitions are hoisted, and the rest is wrapped in one `begin` that ends in
//! a summary string -- so the whole `use` still yields exactly one result value.

use std::path::Path;

use steel::parser::{ast::ExprKind, parser::Parser};

use super::redefine::{self, Kind, Lexeme, Rewritten};

pub(super) struct Expanded {
    pub source: String,
    pub warnings: Vec<String>,
}

/// Expand every top-level `(use "name")`; source without one is returned unchanged.
pub(super) fn expand(
    source: &str,
    library: Option<&Path>,
    rewrite: &dyn Fn(&str) -> Rewritten,
) -> Result<Expanded, String> {
    let unchanged = || Expanded {
        source: source.to_owned(),
        warnings: Vec::new(),
    };
    let Some(lexemes) = redefine::lex(source) else {
        return Ok(unchanged());
    };
    let mut edits = Vec::new();
    let mut warnings = Vec::new();
    let mut depth = 0_usize;
    for (index, lexeme) in lexemes.iter().enumerate() {
        let quoted = index > 0 && lexemes[index - 1].kind == Kind::Prefix;
        if let Some(name) = use_form(&lexemes, index).filter(|_| !quoted) {
            if depth != 0 {
                return Err(format!(
                    "(use \"{name}\") must be a top-level form, not inside another expression"
                ));
            }
            let text = load(library, &name)?;
            let loaded = rewrite(&text);
            warnings.extend(
                loaded
                    .warnings
                    .iter()
                    .map(|warning| format!("{name}.scm: {warning}")),
            );
            edits.push((
                lexeme.start,
                lexemes[index + 3].end,
                wrap(&name, &loaded.source)?,
            ));
        }
        match lexeme.kind {
            Kind::Open | Kind::Literal => depth += 1,
            Kind::Close => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if edits.is_empty() {
        return Ok(unchanged());
    }
    let mut expanded = source.to_owned();
    for (start, end, text) in edits.into_iter().rev() {
        expanded.replace_range(start..end, &text);
    }
    Ok(Expanded {
        source: expanded,
        warnings,
    })
}

/// `(use "name")` starting at `index`: the name, if this is one.
fn use_form(lexemes: &[Lexeme], index: usize) -> Option<String> {
    let [open, head, name, close] = lexemes.get(index..index + 4)? else {
        return None;
    };
    (open.kind == Kind::Open
        && head.kind == Kind::Identifier
        && head.text == "use"
        && name.kind == Kind::Str
        && close.kind == Kind::Close)
        .then(|| name.text.clone())
}

fn load(library: Option<&Path>, name: &str) -> Result<String, String> {
    let Some(directory) = library else {
        return Err(
            "no Scheme library directory is configured; start the server with --scheme-library DIR (or BLENDER_MCP_SCHEME_LIBRARY)"
                .to_owned(),
        );
    };
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!(
            "library names use letters, digits, '-' and '_' only, not {name:?}"
        ));
    }
    let root = directory
        .canonicalize()
        .map_err(|error| format!("Scheme library {}: {error}", directory.display()))?;
    let file = root.join(format!("{name}.scm"));
    match file.canonicalize() {
        // A symlink must not lead out of the library.
        Ok(resolved) if resolved.starts_with(&root) => std::fs::read_to_string(&resolved)
            .map_err(|error| format!("reading {}: {error}", resolved.display())),
        Ok(_) => Err(format!("{name}.scm resolves outside the Scheme library")),
        Err(_) => Err(format!(
            "no {name}.scm in the Scheme library; available: {}",
            available(&root)
        )),
    }
}

fn available(root: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == "scm")
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    names.sort();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Hoist macro definitions (they cannot live inside `begin`) and wrap the rest.
fn wrap(name: &str, text: &str) -> Result<String, String> {
    let lexemes = redefine::lex(text).ok_or_else(|| format!("{name}.scm does not lex"))?;
    let mut macros = String::new();
    let mut rest = String::new();
    let mut copied = 0;
    let mut index = 0;
    while index < lexemes.len() {
        if lexemes[index].kind == Kind::Open
            && lexemes
                .get(index + 1)
                .is_some_and(|next| next.kind == Kind::DefineSyntax)
        {
            let close = redefine::matching_close(&lexemes, index)
                .ok_or_else(|| format!("{name}.scm has an unclosed form"))?;
            rest.push_str(&text[copied..lexemes[index].start]);
            macros.push_str(&text[lexemes[index].start..lexemes[close].end]);
            macros.push('\n');
            copied = lexemes[close].end;
            index = close + 1;
        } else {
            index += 1;
        }
    }
    rest.push_str(&text[copied..]);
    let forms = Parser::parse(text)
        .map_err(|error| format!("{name}.scm: {error}"))?
        .iter()
        .filter(|form| !matches!(form, ExprKind::Macro(_)))
        .count();
    // Comments end at a newline, so keep one before every closing delimiter.
    Ok(format!(
        "{macros}(begin\n{rest}\n\"loaded {name} ({forms} forms)\")"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_rewrite(text: &str) -> Rewritten {
        Rewritten {
            source: text.to_owned(),
            warnings: Vec::new(),
        }
    }

    fn library(files: &[(&str, &str)]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary library");
        for (name, text) in files {
            std::fs::write(directory.path().join(name), text).expect("write library file");
        }
        directory
    }

    #[test]
    fn use_expands_to_one_value_with_macros_hoisted() {
        let directory = library(&[(
            "kit.scm",
            "(define-syntax twice (syntax-rules () [(_ e) (begin e e)]))\n(define (k) 1) ; note\n(define z 2)",
        )]);
        let expanded =
            expand("(+ 1 1) (use \"kit\")", Some(directory.path()), &no_rewrite).expect("expands");
        assert_eq!(
            expanded.source,
            "(+ 1 1) (define-syntax twice (syntax-rules () [(_ e) (begin e e)]))\n(begin\n\n(define (k) 1) ; note\n(define z 2)\n\"loaded kit (2 forms)\")"
        );
    }

    #[test]
    fn use_is_checked() {
        let directory = library(&[("kit.scm", "1"), ("other.scm", "2")]);
        let error = |source: &str, root: Option<&Path>| {
            expand(source, root, &no_rewrite).err().unwrap_or_default()
        };
        assert!(error("(use \"kit\")", None).contains("--scheme-library"));
        assert!(error("(use \"../kit\")", Some(directory.path())).contains("letters, digits"));
        assert!(
            error("(use \"missing\")", Some(directory.path())).contains("available: kit, other")
        );
        assert!(error("(list (use \"kit\"))", Some(directory.path())).contains("top-level"));
        // Strings and quoted data that merely mention use are left alone.
        let text = "(display \"(use \\\"kit\\\")\") '(use x) '(use \"kit\")";
        assert_eq!(
            expand(text, None, &no_rewrite).expect("unchanged").source,
            text
        );
    }
}
