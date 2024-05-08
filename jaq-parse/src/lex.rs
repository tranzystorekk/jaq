use crate::token::{Delim, Token, Tree};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use parcours::{any, lazy, str, Combinator, Parser};

fn strip_digits(i: &str) -> Option<&str> {
    i.strip_prefix(|c: char| c.is_numeric())
        .map(|i| i.trim_start_matches(|c: char| c.is_numeric()))
}

/// Decimal with optional exponent.
fn trim_num(i: &str) -> &str {
    let i = i.trim_start_matches(|c: char| c.is_numeric());
    let i = i.strip_prefix('.').map_or(i, |i| {
        strip_digits(i).unwrap_or_else(|| {
            // TODO: register error
            todo!();
            i
        })
    });
    let i = i.strip_prefix(['e', 'E']).map_or(i, |i| {
        let i = i.strip_prefix(['+', '-']).unwrap_or(i);
        strip_digits(i).unwrap_or_else(|| {
            // TODO: register error
            todo!();
            i
        })
    });
    i
}

fn trim_ident(i: &str) -> &str {
    i.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_')
}

fn strip_ident(i: &str) -> Option<&str> {
    i.strip_prefix(|c: char| c.is_ascii_alphabetic() || c == '_')
        .map(trim_ident)
}

fn token(i: &str) -> Option<(Token, &str)> {
    let is_op = |c| "|=!<>+-*/%".contains(c);
    let prefix = |rest: &str| &i[..i.len() - rest.len()];
    let single = |tk: Token| (tk, &i[1..]);

    let mut chars = i.chars();
    Some(match chars.next()? {
        'a'..='z' | 'A'..='Z' | '@' | '_' => {
            let rest = trim_ident(chars.as_str());
            let tk = match prefix(rest) {
                "def" => Token::Def,
                "if" => Token::If,
                "then" => Token::Then,
                "elif" => Token::Elif,
                "else" => Token::Else,
                "end" => Token::End,
                "or" => Token::Or,
                "and" => Token::And,
                "as" => Token::As,
                "reduce" => Token::Reduce,
                "for" => Token::For,
                "foreach" => Token::Foreach,
                "try" => Token::Try,
                "catch" => Token::Catch,
                ident => Token::Ident(ident.to_string()),
            };
            (tk, rest)
        }
        '$' => {
            // TODO: handle error
            let rest = strip_ident(chars.as_str()).unwrap();
            (Token::Var(i[1..i.len() - rest.len()].to_string()), rest)
        }
        '0'..='9' => {
            let rest = trim_num(chars.as_str());
            (Token::Num(prefix(rest).to_string()), rest)
        }
        '.' if chars.next()? == '.' => (Token::DotDot, &i[2..]),
        '.' => single(Token::Dot),
        ':' => single(Token::Colon),
        ';' => single(Token::Semicolon),
        ',' => single(Token::Comma),
        '?' => single(Token::Question),
        c if is_op(c) => {
            let rest = chars.as_str().trim_start_matches(is_op);
            (Token::Op(prefix(rest).to_string()), rest)
        }
        _ => return None,
    })
}

use jaq_syn::string::Part;

/// Returns `None` when an unexpected EOF was encountered.
fn string(mut i: &str) -> Option<(Vec<Part<Tree>>, &str)> {
    let mut parts = Vec::new();

    loop {
        let rest = i.trim_start_matches(|c| c != '\\' && c != '"');
        parts.push(Part::Str(i[..i.len() - rest.len()].to_string()));
        let mut chars = rest.chars();
        let c = match chars.next()? {
            '"' => return Some((parts, chars.as_str())),
            '\\' => match chars.next()? {
                c @ ('\\' | '/' | '"') => c,
                'b' => '\x08',
                'f' => '\x0C',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let mut hex = String::with_capacity(4);
                    (0..4).try_for_each(|_| Some(hex.push(chars.next()?)))?;
                    let num = u32::from_str_radix(&hex, 16).unwrap();
                    char::from_u32(num).unwrap_or_else(|| {
                        //emit(Simple::custom(span, "invalid unicode character"));
                        '\u{FFFD}' // unicode replacement character
                    })
                }
                '(' => {
                    let (trees, rest) = trees(chars.as_str(), Delim::Paren);
                    parts.push(Part::Fun(trees));
                    i = rest;
                    continue;
                }
                _ => todo!("add error"),
            },
            _ => unreachable!(),
        };
        parts.push(Part::Str(c.into()));
        i = chars.as_str();
    }
}

/// Whitespace and comments.
fn trim_space(i: &str) -> &str {
    let mut i = i.trim_start();
    while let Some(comment) = i.strip_prefix('#') {
        i = comment.trim_start_matches(|c| c != '\n').trim_start();
    }
    i
}

use jaq_syn::Spanned;
fn parts_to_interpol(
    parts: Vec<Part<Tree>>,
) -> (Spanned<String>, Vec<(Spanned<Tree>, Spanned<String>)>) {
    let mut init = (String::new(), 0..42);
    let mut tail = Vec::new();
    let mut parts = parts.into_iter();
    while let Some(part) = parts.next() {
        match part {
            Part::Str(s) => init.0.extend(s.chars()),
            Part::Fun(f) => {
                tail.push(((f, 0..42), (String::new(), 0..42)));
                while let Some(part) = parts.next() {
                    match part {
                        Part::Str(s) => tail.last_mut().unwrap().1 .0.extend(s.chars()),
                        Part::Fun(f) => tail.push(((f, 0..42), (String::new(), 0..42))),
                    }
                }
            }
        }
    }
    (init, tail)
}

fn trees2(mut i: &str) -> (Vec<Spanned<Tree>>, &str) {
    let mut trees = Vec::new();
    while let Some((tree, rest)) = tree_(i) {
        trees.push((tree, 0..42));
        i = rest;
    }
    (trees, i)
}

fn trees(mut i: &str, delim: Delim) -> (Tree, &str) {
    let (trees, i) = trees2(i);
    let i = trim_space(i);
    let i = i.strip_prefix(delim.close()).unwrap_or_else(|| {
        todo!("add error");
        i
    });
    (Tree::Delim(delim, trees), i)
}

fn tree_(i: &str) -> Option<(Tree, &str)> {
    let i = trim_space(i);
    let mut chars = i.chars();

    Some(match chars.next()? {
        '"' => {
            let (parts, rest) = string(chars.as_str())?;
            let (init, tail) = parts_to_interpol(parts);
            (Tree::String(init, tail), rest)
        }
        '(' => trees(chars.as_str(), Delim::Paren),
        '[' => trees(chars.as_str(), Delim::Brack),
        '{' => trees(chars.as_str(), Delim::Brace),
        _ => {
            let (token, rest) = token(i)?;
            (Tree::Token(token), rest)
        }
    })
}

pub fn lex_(i: &str) -> (Vec<Spanned<Tree>>, &str) {
    let (trees, i) = trees2(i);
    let i = trim_space(i);
    (trees, i)
}