//! Finds a Makefile's targets by reading its text. `make` itself is never run for this, because
//! evaluating a Makefile can run its `$(shell ...)` calls, so `include`d and generated targets are
//! knowingly left out.

use std::fs;
use std::path::Path;

/// GNU make's lookup order.
const MAKEFILE_NAMES: [&str; 3] = ["GNUmakefile", "makefile", "Makefile"];

/// Words that open a line that isn't a rule, unless a colon follows, as in `export: deps`.
const DIRECTIVES: [&str; 18] = [
    "ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "-include", "sinclude", "load", "-load", "vpath",
    "export", "unexport", "override", "private", "undefine", "endef",
];

/// Words that can come before a variable assignment or a `define`.
const MODIFIERS: [&str; 3] = ["export", "override", "private"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Makefile {
    /// Without duplicates, in file order.
    pub targets: Vec<String>,
    /// Only when set to a literal name.
    pub default_goal: Option<String>,
}

/// The text of the Makefile that `make` would use in `dir`.
pub fn read(dir: &Path) -> Option<String> {
    MAKEFILE_NAMES
        .iter()
        .find_map(|name| fs::read(dir.join(name)).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

pub fn parse(text: &str) -> Makefile {
    let mut parsed = Makefile::default();
    let mut define_depth = 0;
    for line in logical_lines(text) {
        if line.starts_with('\t') {
            continue;
        }
        if define_depth > 0 {
            match line.split_whitespace().next() {
                Some("define") => define_depth += 1,
                Some("endef") => define_depth -= 1,
                _ => {}
            }
            continue;
        }
        let line = without_comment(&line).trim();
        let bare = without_modifiers(line);
        if opens_define(bare) {
            define_depth = 1;
        } else if let Some((operator, value)) = default_goal_assignment(bare) {
            apply_default_goal(&mut parsed.default_goal, operator, value);
        } else if !is_directive(line) {
            for name in rule_targets(line).into_iter().filter(|name| is_listable(name)) {
                if !parsed.targets.iter().any(|known| known == name) {
                    parsed.targets.push(name.to_string());
                }
            }
        }
    }
    parsed
}

/// The lines after joining backslash continuations. An even run of backslashes doesn't continue.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let backslashes = raw.len() - raw.trim_end_matches('\\').len();
        if backslashes % 2 == 1 {
            pending.push_str(&raw[..raw.len() - 1]);
            pending.push(' ');
        } else {
            pending.push_str(raw);
            lines.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    lines
}

/// Each character that's outside any `$(...)` or `${...}` and isn't backslash-escaped, with its
/// byte offset. Only these can separate names or mark a rule, an assignment or a comment.
fn top_level(text: &str) -> Vec<(usize, char)> {
    let mut found = Vec::new();
    let mut depth = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' | '$' if depth == 0 => {
                if c == '$' && matches!(chars.peek(), Some((_, '(' | '{'))) {
                    depth = 1;
                }
                chars.next();
            }
            '(' | '{' if depth > 0 => depth += 1,
            ')' | '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => found.push((at, c)),
            _ => {}
        }
    }
    found
}

fn without_comment(line: &str) -> &str {
    match top_level(line).iter().find(|(_, c)| *c == '#') {
        Some((at, _)) => &line[..*at],
        None => line,
    }
}

/// Splits on whitespace, keeping a `$(...)` with spaces in it as part of one word.
fn words(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut start = 0;
    for (at, c) in top_level(text) {
        if c.is_whitespace() {
            found.push(&text[start..at]);
            start = at + c.len_utf8();
        }
    }
    found.push(&text[start..]);
    found.retain(|word| !word.is_empty());
    found
}

/// The first word of `line` and what follows it, both trimmed.
fn first_word(line: &str) -> (&str, &str) {
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    (word, rest.trim_start())
}

fn without_modifiers(mut line: &str) -> &str {
    loop {
        let (word, rest) = first_word(line);
        if !MODIFIERS.contains(&word) || rest.is_empty() || rest.starts_with([':', '=']) {
            return line;
        }
        line = rest;
    }
}

/// `define = 1` and `define: deps` use `define` as an ordinary name.
fn opens_define(line: &str) -> bool {
    let (word, rest) = first_word(line);
    word == "define" && !rest.starts_with([':', '=', '?', '+', '!'])
}

fn is_directive(line: &str) -> bool {
    let (word, rest) = first_word(line);
    DIRECTIVES.contains(&word) && !rest.starts_with(':')
}

/// The operator and value if this line assigns `.DEFAULT_GOAL`.
fn default_goal_assignment(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix(".DEFAULT_GOAL")?.trim_start();
    [":::=", "::=", ":=", "?=", "+=", "="]
        .iter()
        .find_map(|operator| rest.strip_prefix(*operator).map(|value| (*operator, value.trim())))
}

fn apply_default_goal(goal: &mut Option<String>, operator: &str, value: &str) {
    match operator {
        "?=" if goal.is_some() => {}
        // Appending would make it two goals, which make itself refuses.
        "+=" => {}
        _ => {
            let literal = !value.is_empty() && !value.contains('$') && !value.contains(char::is_whitespace);
            *goal = literal.then(|| value.to_string());
        }
    }
}

/// The names before the colon, or nothing if this line isn't a rule. That rules out every form of
/// assignment, including target-specific ones such as `foo: VAR = 1`.
fn rule_targets(line: &str) -> Vec<&str> {
    let top = top_level(line);
    let Some(&(colon, ':')) = top.iter().find(|(_, c)| matches!(c, ':' | '=')) else { return Vec::new() };
    let after = &line[colon + 1..];
    if after.trim_start_matches(':').starts_with('=') {
        return Vec::new();
    }
    // A `;` starts a recipe, so an `=` beyond it belongs to the shell.
    let assigns = top_level(after).into_iter().map(|(_, c)| c).take_while(|c| *c != ';').any(|c| c == '=');
    if assigns {
        return Vec::new();
    }
    // `a b &: deps` is a grouped rule, and the `&` isn't a name.
    let names = line[..colon].trim_end();
    words(names.strip_suffix('&').unwrap_or(names))
}

fn is_listable(name: &str) -> bool {
    let escaped = ["\\:", "\\ ", "\\#"].iter().any(|pair| name.contains(pair));
    !escaped && !name.contains(['%', '$']) && !name.starts_with(['.', '-'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(text: &str) -> Vec<String> {
        parse(text).targets
    }

    #[test]
    fn simple_and_multi_target_rules_list_each_name() {
        assert_eq!(targets("build:\n\tcargo build\nrun test: build\n"), ["build", "run", "test"]);
        assert_eq!(targets("spaced : deps\n  indented: deps\n"), ["spaced", "indented"]);
        assert_eq!(targets("static.o other.o: %.o: %.c\n"), ["static.o", "other.o"]);
        assert_eq!(targets("one two &: deps\nthree&: deps\n"), ["one", "two", "three"]);
    }

    #[test]
    fn double_colon_rules_count_once() {
        assert_eq!(targets("all:: one\n\techo 1\nall:: two\n\techo 2\n"), ["all"]);
    }

    #[test]
    fn continuations_are_joined_before_parsing() {
        assert_eq!(targets("one \\\n  two: deps \\\n  more\nthree:\n"), ["one", "two", "three"]);
        assert_eq!(targets("VAR = a \\\n  fake: b\nreal:\n"), ["real"], "the second line is part of the value");
        assert_eq!(targets("# comment \\\nfake:\nreal:\n"), ["real"], "a comment continues too");
        assert_eq!(targets("real:\n\techo a \\\nfake: b\n"), ["real"], "so does a recipe");
        assert_eq!(targets("# two backslashes \\\\\nreal:\n"), ["real"], "an even run doesn't continue");
        assert_eq!(targets("real:\r\n\techo\r\nother: \\\r\n  dep\r\n"), ["real", "other"]);
    }

    #[test]
    fn comments_and_recipes_are_ignored() {
        assert_eq!(targets("# fake: deps\n  # fake2: deps\nreal: deps # fake3: deps\n"), ["real"]);
        assert_eq!(targets("real:\n\tfake: deps\n\t@other: thing\n"), ["real"]);
    }

    #[test]
    fn define_blocks_are_ignored_including_nested_ones() {
        let text = "define BLOCK\nfake: deps\nendef\nreal:\n\
                    define OUTER =\ndefine INNER\nfake2:\nendef\nfake3:\nendef # done\nlast:\n\
                    override define OVER\nfake4:\nendef\nend:\n";
        assert_eq!(targets(text), ["real", "last", "end"]);
    }

    #[test]
    fn define_as_an_ordinary_name_opens_nothing() {
        assert_eq!(targets("define: deps\nnext:\n"), ["define", "next"]);
        assert_eq!(targets("define = 1\nnext:\n"), ["next"]);
    }

    #[test]
    fn assignments_are_not_targets() {
        let text = "A = 1:2\nB := 1\nC ::= 1\nD :::= 1\nE ?= 1:2\nF += 1:2\nG != echo a:b\nH:=1\nI::=1\nexport J := 1\n\
                    override K = 1:2\nprivate L = 1\nreal:\n";
        assert_eq!(targets(text), ["real"]);
    }

    #[test]
    fn target_specific_variables_are_ignored_and_the_rule_still_lists_it() {
        let text = "foo: VAR = 1\nbar: export VAR := 1\nbaz:: VAR += 1\nqux: private VAR ?= 1\nsquashed: VAR=1\nfoo:\n\techo\n";
        assert_eq!(targets(text), ["foo"]);
    }

    #[test]
    fn an_equals_sign_in_an_inline_recipe_or_a_reference_does_not_make_an_assignment() {
        assert_eq!(targets("inline: deps ; echo A=1\nnone: ; echo B=2\n"), ["inline", "none"]);
        assert_eq!(targets("ref: $(if $(X),a=b)\n"), ["ref"]);
    }

    #[test]
    fn pattern_and_variable_names_are_excluded() {
        assert_eq!(targets("%.o: %.c\nlib%: x\n$(OUT): x\nout-$(ARCH): x\n${OUT}: y\nreal: x\n"), ["real"]);
        assert_eq!(targets("kept $(A) $(B C): deps\n"), ["kept"], "a reference with a space is still one word");
        assert_eq!(targets("kept $(FILES:.c=.o): deps\n"), ["kept"], "a colon in a reference isn't the rule's");
        assert_eq!(targets("$(info a: b)\n$(error c: d)\nreal:\n"), ["real"]);
    }

    #[test]
    fn dot_and_dash_prefixed_names_are_excluded() {
        assert_eq!(targets(".PHONY: real fake\n.SUFFIXES:\n.hidden:\n./path:\n-flag:\nreal:\n"), ["real"]);
    }

    #[test]
    fn escaped_names_are_excluded() {
        assert_eq!(targets("with\\:colon: deps\nwith\\ space: deps\nreal: deps\n"), ["real"]);
    }

    #[test]
    fn directives_are_not_rules_but_can_be_names() {
        let text = "ifeq ($(OS),a:b)\nin-if:\nelse ifeq (x,c:d)\nin-else:\nelse\nendif\n\
                    include other.mk:more\n-include x:y\nvpath %.c src:lib\nifdef A:B\nendif\n";
        assert_eq!(targets(text), ["in-if", "in-else"]);
        assert_eq!(targets("include: deps\nexport: deps\nelse : deps\n"), ["include", "export", "else"]);
    }

    #[test]
    fn duplicates_keep_their_first_position() {
        assert_eq!(targets("b:\na:\nb: more\nc a:\n"), ["b", "a", "c"]);
    }

    #[test]
    fn included_targets_are_knowingly_not_followed() {
        assert_eq!(targets("include rules.mk\nreal:\n"), ["real"]);
    }

    #[test]
    fn the_default_goal_is_read_when_literal() {
        assert_eq!(parse(".DEFAULT_GOAL := build\nbuild:\n").default_goal.as_deref(), Some("build"));
        assert_eq!(parse(".DEFAULT_GOAL = build # why\n").default_goal.as_deref(), Some("build"));
        assert_eq!(parse(".DEFAULT_GOAL ::= build\n").default_goal.as_deref(), Some("build"));
        assert_eq!(parse("override .DEFAULT_GOAL := build\n").default_goal.as_deref(), Some("build"));
        assert_eq!(parse(".DEFAULT_GOAL := one\n.DEFAULT_GOAL := two\n").default_goal.as_deref(), Some("two"));
        assert_eq!(parse(".DEFAULT_GOAL := one\n.DEFAULT_GOAL ?= two\n").default_goal.as_deref(), Some("one"));
        assert_eq!(parse(".DEFAULT_GOAL ?= two\n").default_goal.as_deref(), Some("two"));
    }

    #[test]
    fn a_default_goal_that_is_not_literal_is_unknown() {
        assert_eq!(parse("build:\n").default_goal, None);
        assert_eq!(parse(".DEFAULT_GOAL := $(GOAL)\n").default_goal, None);
        assert_eq!(parse(".DEFAULT_GOAL := one\n.DEFAULT_GOAL :=\n").default_goal, None);
        assert_eq!(parse("\t.DEFAULT_GOAL := recipe\n").default_goal, None);
        assert_eq!(parse("define X\n.DEFAULT_GOAL := block\nendef\n").default_goal, None);
        assert_eq!(parse(".DEFAULT_GOALS := other\n").default_goal, None);
        assert!(parse(".DEFAULT_GOAL := build\n").targets.is_empty());
    }

    #[test]
    fn this_repos_makefile_lists_what_it_documents() {
        let parsed = parse(include_str!("../Makefile"));
        assert_eq!(parsed.targets, ["build", "run", "release", "test", "fmt", "fmt-check", "lint", "check", "clean"]);
        assert_eq!(parsed.default_goal.as_deref(), Some("build"));
    }

    #[test]
    fn lookup_follows_gnu_makes_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
        // Written from last to first, so this also holds on a case-insensitive filesystem.
        fs::write(dir.path().join("Makefile"), "third:\n").unwrap();
        assert_eq!(read(dir.path()).as_deref(), Some("third:\n"));
        fs::write(dir.path().join("makefile"), "second:\n").unwrap();
        assert_eq!(read(dir.path()).as_deref(), Some("second:\n"));
        fs::write(dir.path().join("GNUmakefile"), "first:\n").unwrap();
        assert_eq!(read(dir.path()).as_deref(), Some("first:\n"));
    }

    #[test]
    fn a_directory_named_makefile_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("GNUmakefile")).unwrap();
        fs::write(dir.path().join("Makefile"), "real:\n").unwrap();
        assert_eq!(read(dir.path()).as_deref(), Some("real:\n"));
    }
}
