//! Top-level redefinitions update existing bindings.
//!
//! Steel gives every `define` a new global slot, so a function defined in an earlier
//! evaluation keeps calling the *old* binding after a later evaluation redefines a
//! helper -- the opposite of what a REPL user expects, and the reason authoring
//! sessions resorted to resetting and reloading whole toolkits. A top-level `define`
//! of a name the user already defined is therefore compiled as `set!`, which updates
//! the existing slot for every caller. Builtin and stdlib names are never rewritten:
//! redefining `cos` must not change what the stdlib calls, so those get a warning.

use steel::parser::{lexer::TokenStream, tokens::TokenType};

pub(super) struct Rewritten {
    pub source: String,
    pub warnings: Vec<String>,
}

pub(super) struct Lexeme {
    pub kind: Kind,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Open,
    /// `#(` and `#u8(`: vector and byte literals open a form that is data.
    Literal,
    Close,
    /// The `define` keyword, which Steel lexes as its own token.
    Define,
    DefineSyntax,
    /// A string literal; `text` holds its value.
    Str,
    Identifier,
    /// Quote, quasiquote, syntax quotes and datum comments: the next form is data.
    Prefix,
    Other,
}

pub(super) fn lex(source: &str) -> Option<Vec<Lexeme>> {
    TokenStream::new(source, true, None)
        .map(|token| {
            let token = token.ok()?;
            let kind = match token.ty {
                TokenType::OpenParen(_, None) => Kind::Open,
                TokenType::OpenParen(_, Some(_)) => Kind::Literal,
                TokenType::CloseParen(_) => Kind::Close,
                TokenType::Define => Kind::Define,
                TokenType::DefineSyntax => Kind::DefineSyntax,
                TokenType::Identifier(_) => Kind::Identifier,
                TokenType::QuoteTick
                | TokenType::QuasiQuote
                | TokenType::QuoteSyntax
                | TokenType::QuasiQuoteSyntax
                | TokenType::DatumComment => Kind::Prefix,
                TokenType::StringLiteral(_) => Kind::Str,
                _ => Kind::Other,
            };
            let text = match &token.ty {
                TokenType::StringLiteral(value) => value.resolve().to_owned(),
                _ => token.source.to_owned(),
            };
            Some(Lexeme {
                kind,
                text,
                start: token.span.start as usize,
                end: token.span.end as usize,
            })
        })
        .collect()
}

/// Rewrite top-level `define`s of existing user names into `set!`. Anything that does
/// not lex is returned unchanged, so Steel reports the real syntax error.
pub(super) fn rewrite(
    source: &str,
    is_user: impl Fn(&str) -> bool,
    is_builtin: impl Fn(&str) -> bool,
) -> Rewritten {
    let unchanged = || Rewritten {
        source: source.to_owned(),
        warnings: Vec::new(),
    };
    let Some(lexemes) = lex(source) else {
        return unchanged();
    };
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut warnings = Vec::new();
    let mut index = 0;
    let mut quoted = false;
    while index < lexemes.len() {
        let lexeme = &lexemes[index];
        if lexeme.kind == Kind::Prefix {
            quoted = true;
            index += 1;
            continue;
        }
        if lexeme.kind == Kind::Literal {
            let Some(close) = matching_close(&lexemes, index) else {
                return unchanged();
            };
            quoted = false;
            index = close + 1;
            continue;
        }
        if lexeme.kind != Kind::Open {
            index += 1;
            quoted = false;
            continue;
        }
        let Some(close) = matching_close(&lexemes, index) else {
            return unchanged();
        };
        if !quoted {
            redefinition(
                &lexemes,
                index,
                close,
                &is_user,
                &is_builtin,
                &mut edits,
                &mut warnings,
            );
        }
        quoted = false;
        index = close + 1;
    }
    let mut rewritten = source.to_owned();
    edits.sort_by_key(|(start, ..)| std::cmp::Reverse(*start));
    for (start, end, text) in edits {
        rewritten.replace_range(start..end, &text);
    }
    Rewritten {
        source: rewritten,
        warnings,
    }
}

pub(super) fn matching_close(lexemes: &[Lexeme], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, lexeme) in lexemes[open..].iter().enumerate() {
        match lexeme.kind {
            Kind::Open | Kind::Literal => depth += 1,
            Kind::Close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Queue edits for one top-level form `(define ...)` spanning `open..=close`.
fn redefinition(
    lexemes: &[Lexeme],
    open: usize,
    close: usize,
    is_user: &impl Fn(&str) -> bool,
    is_builtin: &impl Fn(&str) -> bool,
    edits: &mut Vec<(usize, usize, String)>,
    warnings: &mut Vec<String>,
) {
    let token = |offset: usize| lexemes.get(open + offset).filter(|_| open + offset < close);
    let Some(keyword) = token(1).filter(|t| t.kind == Kind::Define) else {
        return;
    };
    // (define name expr) or (define (name args ...) body ...); curried and other
    // shapes are left to Steel.
    let (name, function) = match (token(2), token(3)) {
        (Some(name), Some(_)) if name.kind == Kind::Identifier => (name, false),
        (Some(open_args), Some(name))
            if open_args.kind == Kind::Open && name.kind == Kind::Identifier =>
        {
            (name, true)
        }
        _ => return,
    };
    if is_builtin(&name.text) {
        warnings.push(format!(
            "`{}` is a builtin or stdlib name: this define makes a new binding that code defined earlier, including the stdlib, does not see; prefer another name",
            name.text
        ));
        return;
    }
    if !is_user(&name.text) {
        return;
    }
    let end = &lexemes[close];
    if function {
        edits.push((
            keyword.start,
            name.end,
            format!("begin (set! {} (lambda (", name.text),
        ));
        edits.push((end.start, end.end, ")) (if #f #f))".to_owned()));
    } else {
        edits.push((keyword.start, keyword.end, "begin (set!".to_owned()));
        edits.push((end.start, end.end, ") (if #f #f))".to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str) -> Rewritten {
        rewrite(
            source,
            |name| matches!(name, "f" | "x" | "hand-joints"),
            |name| matches!(name, "cos" | "look-at!"),
        )
    }

    #[test]
    fn existing_user_definitions_become_set() {
        assert_eq!(
            run("(define x (+ 1 2))").source,
            "(begin (set! x (+ 1 2)) (if #f #f))"
        );
        assert_eq!(
            run("(define (f a [b 1]) \"doc\" (+ a b))").source,
            "(begin (set! f (lambda ( a [b 1]) \"doc\" (+ a b))) (if #f #f))"
        );
        // Wrap and replace in one evaluation: the old value is read before the set!.
        assert_eq!(
            run("(define HJ hand-joints) (define (hand-joints s) (HJ s))").source,
            "(define HJ hand-joints) (begin (set! hand-joints (lambda ( s) (HJ s))) (if #f #f))"
        );
    }

    #[test]
    fn new_names_quoted_forms_and_nested_defines_are_untouched() {
        for source in [
            "(define fresh 1)",
            "'(define x 1)",
            "`(define x 1)",
            "#;(define x 1) 2",
            "(let () (define x 1) x)",
            "(define ((f a) b) a)",
            "(define x)",
            "; (define x 1)\n(+ 1 1)",
            "#(define x 1)",
        ] {
            assert_eq!(run(source).source, source, "{source}");
        }
    }

    #[test]
    fn builtins_warn_and_stay_defines() {
        let result = run("(define (cos v) v) (define look-at! 1)");
        assert_eq!(result.source, "(define (cos v) v) (define look-at! 1)");
        assert_eq!(result.warnings.len(), 2);
        assert!(result.warnings[0].contains("`cos`"));
    }

    #[test]
    fn unlexable_source_is_left_for_steel_to_report() {
        assert_eq!(run("(define x (+ 1").source, "(define x (+ 1");
    }
}
