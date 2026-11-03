//! Starter Makefiles. Every project's test gate is `make test`, so each one needs a Makefile.

use std::path::Path;

const RUST: &str = "\
# Requires GNU Make (`winget install ezwinports.make`).
#
#   make            debug build
#   make run        build and launch
#   make release    optimized build
#   make test       unit tests
#   make fmt        format the code
#   make lint       clippy, warnings are errors
#   make check      fmt-check + lint + test
#   make clean      remove target/

.DEFAULT_GOAL := build
.PHONY: build run release test fmt fmt-check lint check clean

build:
\tcargo build

run:
\tcargo run

release:
\tcargo build --release

test:
\tcargo test

fmt:
\tcargo fmt

fmt-check:
\tcargo fmt --check

lint:
\tcargo clippy --all-targets -- -D warnings

check: fmt-check lint test

clean:
\tcargo clean
";

const NODE: &str = "\
.DEFAULT_GOAL := test
.PHONY: install build test

install:
\tnpm ci

build:
\tnpm run build

test:
\tnpm test
";

const GO: &str = "\
.DEFAULT_GOAL := build
.PHONY: build test

build:
\tgo build ./...

test:
\tgo test ./...
";

const PYTHON: &str = "\
.DEFAULT_GOAL := test
.PHONY: test

test:
\tpython -m pytest
";

/// Fails on purpose: a gate that passes with no tests would wave every change through.
const UNKNOWN: &str = "\
.DEFAULT_GOAL := test
.PHONY: test

test:
\t@echo \"No test suite is set up yet. Edit the test target in this Makefile.\"
\t@exit 1
";

pub fn template(dir: &Path) -> &'static str {
    let has = |file: &str| dir.join(file).exists();
    if has("Cargo.toml") {
        RUST
    } else if has("package.json") {
        NODE
    } else if has("go.mod") {
        GO
    } else if has("pyproject.toml") || has("pytest.ini") || has("tests") {
        PYTHON
    } else {
        UNKNOWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn picks_a_template_from_the_project_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(template(dir.path()), UNKNOWN);
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(template(dir.path()), NODE);
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(template(dir.path()), RUST);
    }

    #[test]
    fn every_template_has_a_tab_indented_test_target() {
        for text in [RUST, NODE, GO, PYTHON, UNKNOWN] {
            let (_, after) = text.split_once("\ntest:\n").expect("test target");
            assert!(after.starts_with('\t'), "{text}");
        }
    }
}
