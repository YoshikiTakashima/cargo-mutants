# Welcome to cargo-mutants

cargo-mutants is a mutation testing tool for Rust. It helps you improve your
program's quality by finding functions whose body could be replaced without
causing any tests to fail. Each such case indicates, perhaps, a gap in semantic
code coverage by your tests, where a bug might be lurking.

**The goal of cargo-mutants is to be _easy_ to run on any Rust source tree, and
to tell you something _interesting_ about areas where bugs might be lurking or
the tests might be insufficient.** ([More about these goals](goals.md).)

To get started:

1. [Install cargo-mutants](install.md).
2. [Run `cargo mutants](getting-started.md) in your Rust source tree.

For more resources see the repository at
<https://github.com/sourcefrog/cargo-mutants>.
