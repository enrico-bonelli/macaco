//! Language definitions and file type detection.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Plain,
    C,
    Cpp,
    Rust,
    Php,
    Bash,
    Python,
    JavaScript,
    TypeScript,
    Yaml,
    Html,
    Css,
    Java,
    Go,
    Markdown,
    Sql,
}

pub const ALL: [Lang; 16] = [
    Lang::Plain,
    Lang::C,
    Lang::Cpp,
    Lang::Rust,
    Lang::Php,
    Lang::Bash,
    Lang::Python,
    Lang::JavaScript,
    Lang::TypeScript,
    Lang::Yaml,
    Lang::Html,
    Lang::Css,
    Lang::Java,
    Lang::Go,
    Lang::Markdown,
    Lang::Sql,
];

/// Lexical description used by the generic lexer.
pub struct Spec {
    pub line_comment: &'static [&'static str],
    pub block_comment: Option<(&'static str, &'static str)>,
    pub keywords: &'static [&'static str],
    pub types: &'static [&'static str],
    pub constants: &'static [&'static str],
    pub builtins: &'static [&'static str],
    /// Characters that open a string literal.
    pub quotes: &'static [u8],
    /// Quotes whose literals may span several lines.
    pub multiline_quotes: &'static [u8],
    /// Quotes that may be tripled (`"""`) to open a multi-line literal.
    pub triple_quotes: &'static [u8],
    pub preprocessor: bool,
    /// `$name` variables (shell, PHP).
    pub dollar_vars: bool,
    /// `@name` decorators / annotations.
    pub decorators: bool,
    /// Identifiers starting with an upper case letter are types.
    pub cap_types: bool,
    pub case_insensitive_kw: bool,
    /// Types, constants and functions are case-insensitive too (SQL).
    pub case_insensitive_words: bool,
    pub rust: bool,
    pub shell: bool,
}

const NONE: &[&str] = &[];

const C_KW: &[&str] = &[
    "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for",
    "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static", "struct",
    "switch", "typedef", "union", "volatile", "while", "_Alignas", "_Alignof", "_Atomic",
    "_Generic", "_Noreturn", "_Static_assert", "_Thread_local", "alignas", "alignof",
    "static_assert", "thread_local", "typeof",
];
const C_TYPES: &[&str] = &[
    "bool", "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void",
    "size_t", "ssize_t", "ptrdiff_t", "intptr_t", "uintptr_t", "int8_t", "int16_t", "int32_t",
    "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "FILE", "_Bool", "wchar_t",
    "off_t", "pid_t",
];
const C_CONST: &[&str] = &["NULL", "true", "false", "EOF", "stdin", "stdout", "stderr"];

const CPP_KW: &[&str] = &[
    "auto", "break", "case", "catch", "class", "const", "constexpr", "consteval", "constinit",
    "const_cast", "continue", "co_await", "co_return", "co_yield", "decltype", "default",
    "delete", "do", "dynamic_cast", "else", "enum", "explicit", "export", "extern", "final",
    "for", "friend", "goto", "if", "inline", "mutable", "namespace", "new", "noexcept",
    "operator", "override", "private", "protected", "public", "register", "reinterpret_cast",
    "requires", "return", "sizeof", "static", "static_assert", "static_cast", "struct", "switch",
    "template", "this", "throw", "try", "typedef", "typeid", "typename", "union", "using",
    "virtual", "volatile", "while", "concept", "alignas", "alignof", "thread_local", "module",
    "import",
];
const CPP_TYPES: &[&str] = &[
    "bool", "char", "char8_t", "char16_t", "char32_t", "double", "float", "int", "long", "short",
    "signed", "unsigned", "void", "wchar_t", "size_t", "ssize_t", "int8_t", "int16_t", "int32_t",
    "int64_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "string", "vector", "map",
    "unordered_map", "set", "unique_ptr", "shared_ptr", "weak_ptr", "std", "array", "optional",
    "pair", "tuple", "string_view",
];
const CPP_CONST: &[&str] = &["nullptr", "NULL", "true", "false"];

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
    "unsafe", "use", "where", "while", "yield", "macro_rules", "union", "gen",
];
const RUST_TYPES: &[&str] = &[
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
    "i128", "isize", "f32", "f64",
];
const RUST_CONST: &[&str] = &["true", "false", "None", "Some", "Ok", "Err"];

const PHP_KW: &[&str] = &[
    "abstract", "and", "array", "as", "break", "callable", "case", "catch", "class", "clone",
    "const", "continue", "declare", "default", "do", "echo", "else", "elseif", "empty",
    "enddeclare", "endfor", "endforeach", "endif", "endswitch", "endwhile", "enum", "extends",
    "final", "finally", "fn", "for", "foreach", "function", "global", "goto", "if",
    "implements", "include", "include_once", "instanceof", "insteadof", "interface", "isset",
    "list", "match", "namespace", "new", "or", "print", "private", "protected", "public",
    "readonly", "require", "require_once", "return", "static", "switch", "throw", "trait", "try",
    "unset", "use", "var", "while", "xor", "yield", "die", "exit",
];
const PHP_TYPES: &[&str] = &[
    "int", "float", "bool", "string", "void", "mixed", "object", "iterable", "never", "self",
    "parent",
];
const PHP_CONST: &[&str] = &["true", "false", "null", "TRUE", "FALSE", "NULL", "__DIR__", "__FILE__", "__LINE__", "__CLASS__", "__FUNCTION__"];

const BASH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done",
    "in", "function", "select", "time", "return", "exit", "break", "continue", "local",
    "export", "readonly", "declare", "typeset", "unset", "shift", "source", "alias", "eval",
    "exec", "trap", "set",
];
const BASH_BUILTINS: &[&str] = &[
    "echo", "printf", "read", "cd", "pwd", "test", "true", "false", "let", "getopts", "kill",
    "wait", "jobs", "umask", "type", "command", "builtin", "mapfile", "readarray", "pushd",
    "popd", "dirs", "shopt", "hash", "ulimit", "exit",
];

const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "match",
    "case", "self", "cls",
];
const PY_TYPES: &[&str] = &[
    "int", "float", "str", "bytes", "bool", "list", "dict", "set", "tuple", "frozenset",
    "object", "complex", "bytearray", "type", "Exception",
];
const PY_CONST: &[&str] = &["True", "False", "None", "NotImplemented", "Ellipsis", "__name__"];
const PY_BUILTINS: &[&str] = &[
    "print", "len", "range", "open", "isinstance", "super", "enumerate", "zip", "map", "filter",
    "sorted", "reversed", "min", "max", "sum", "abs", "any", "all", "repr", "hasattr",
    "getattr", "setattr", "iter", "next", "input", "id", "hash", "format", "vars", "dir",
];

const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
    "default", "delete", "do", "else", "export", "extends", "finally", "for", "from", "function",
    "get", "if", "import", "in", "instanceof", "let", "new", "of", "return", "set", "static",
    "super", "switch", "this", "throw", "try", "typeof", "var", "void", "while", "with", "yield",
];
const JS_CONST: &[&str] = &["true", "false", "null", "undefined", "NaN", "Infinity"];
const JS_BUILTINS: &[&str] = &["console", "window", "document", "JSON", "Math", "Promise", "require", "module", "exports"];

const TS_KW: &[&str] = &[
    "abstract", "as", "async", "await", "break", "case", "catch", "class", "const", "continue",
    "declare", "default", "delete", "do", "else", "enum", "export", "extends", "finally", "for",
    "from", "function", "get", "if", "implements", "import", "in", "infer", "instanceof",
    "interface", "is", "keyof", "let", "module", "namespace", "new", "of", "private",
    "protected", "public", "readonly", "return", "satisfies", "set", "static", "super",
    "switch", "this", "throw", "try", "type", "typeof", "var", "void", "while", "with", "yield",
];
const TS_TYPES: &[&str] = &[
    "any", "boolean", "number", "string", "symbol", "bigint", "unknown", "never", "object",
    "void",
];

const JAVA_KW: &[&str] = &[
    "abstract", "assert", "break", "case", "catch", "class", "const", "continue", "default",
    "do", "else", "enum", "extends", "final", "finally", "for", "goto", "if", "implements",
    "import", "instanceof", "interface", "native", "new", "package", "private", "protected",
    "public", "record", "return", "sealed", "permits", "static", "strictfp", "super", "switch",
    "synchronized", "this", "throw", "throws", "transient", "try", "var", "volatile", "while",
    "yield",
];
const JAVA_TYPES: &[&str] = &[
    "boolean", "byte", "char", "double", "float", "int", "long", "short", "void",
];
const JAVA_CONST: &[&str] = &["true", "false", "null"];

const GO_KW: &[&str] = &[
    "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
    "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
    "return", "select", "struct", "switch", "type", "var",
];
const GO_TYPES: &[&str] = &[
    "bool", "byte", "complex64", "complex128", "error", "float32", "float64", "int", "int8",
    "int16", "int32", "int64", "rune", "string", "uint", "uint8", "uint16", "uint32", "uint64",
    "uintptr", "any",
];
const GO_CONST: &[&str] = &["true", "false", "nil", "iota"];
const GO_BUILTINS: &[&str] = &[
    "append", "cap", "close", "copy", "delete", "len", "make", "new", "panic", "print",
    "println", "recover", "min", "max", "clear",
];

const SQL_KW: &[&str] = &[
    "action", "add", "after", "all", "alter", "always", "analyze", "and", "any", "as", "asc",
    "auto_increment", "autoincrement", "before", "begin", "between", "by", "call", "cascade",
    "case", "cast", "check", "collate", "column", "comment", "commit", "conflict", "constraint",
    "create", "cross", "database", "declare", "default", "deferrable", "deferred", "delete",
    "desc", "describe", "distinct", "do", "drop", "each", "else", "elsif", "end", "escape",
    "except", "exception", "execute", "exists", "explain", "extension", "fetch", "filter",
    "first", "following", "for", "foreign", "from", "full", "function", "generated", "grant",
    "group", "having", "identity", "if", "ilike", "immediate", "immutable", "in", "index",
    "initially", "inner", "insert", "instead", "intersect", "into", "is", "join", "key",
    "language", "last", "lateral", "left", "like", "limit", "loop", "matched", "materialized",
    "merge", "natural", "next", "no", "not", "nothing", "nulls", "of", "offset", "on", "only",
    "or", "order", "outer", "over", "owner", "partition", "perform", "preceding", "primary",
    "procedure", "raise", "range", "recursive", "references", "rename", "replace", "restrict",
    "return", "returning", "returns", "revoke", "right", "role", "rollback", "row", "rows",
    "savepoint", "schema", "security", "select", "sequence", "set", "setof", "show", "similar",
    "some", "stable", "stored", "strict", "table", "tablespace", "temp", "temporary", "then",
    "to", "top", "transaction", "trigger", "truncate", "unbounded", "union", "unique", "update",
    "use", "user", "using", "vacuum", "values", "view", "virtual", "volatile", "when", "where",
    "while", "window", "with", "within",
];
const SQL_TYPES: &[&str] = &[
    "array", "bigint", "bigserial", "binary", "bit", "blob", "bool", "boolean", "bytea", "char",
    "character", "cidr", "clob", "date", "datetime", "decimal", "double", "enum", "float",
    "inet", "int", "integer", "interval", "json", "jsonb", "longtext", "macaddr", "mediumint",
    "mediumtext", "money", "nchar", "numeric", "nvarchar", "precision", "real", "record",
    "serial", "smallint", "smallserial", "text", "time", "timestamp", "timestamptz", "tinyint",
    "tinytext", "tsvector", "uuid", "varbinary", "varchar", "varying", "void", "xml", "year",
    "zone",
];
const SQL_CONST: &[&str] = &[
    "null", "true", "false", "unknown", "current_date", "current_time", "current_timestamp",
    "current_user", "session_user", "localtime", "localtimestamp",
];
const SQL_BUILTINS: &[&str] = &[
    "abs", "array_agg", "avg", "coalesce", "concat", "count", "date_trunc", "dense_rank",
    "extract", "greatest", "lag", "lead", "least", "length", "lower", "max", "min", "now",
    "nullif", "rank", "round", "row_number", "string_agg", "substring", "sum", "to_char",
    "to_date", "trim", "upper",
];

const BASE: Spec = Spec {
    line_comment: NONE,
    block_comment: None,
    keywords: NONE,
    types: NONE,
    constants: NONE,
    builtins: NONE,
    quotes: b"\"'",
    multiline_quotes: b"",
    triple_quotes: b"",
    preprocessor: false,
    dollar_vars: false,
    decorators: false,
    cap_types: false,
    case_insensitive_kw: false,
    case_insensitive_words: false,
    rust: false,
    shell: false,
};

static PLAIN: Spec = BASE;
static C: Spec = Spec {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: C_KW,
    types: C_TYPES,
    constants: C_CONST,
    preprocessor: true,
    ..BASE
};
static CPP: Spec = Spec { keywords: CPP_KW, types: CPP_TYPES, constants: CPP_CONST, cap_types: true, ..C };
static RUST: Spec = Spec {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: RUST_KW,
    types: RUST_TYPES,
    constants: RUST_CONST,
    quotes: b"\"",
    multiline_quotes: b"\"",
    cap_types: true,
    rust: true,
    ..BASE
};
static PHP: Spec = Spec {
    line_comment: &["//", "#"],
    block_comment: Some(("/*", "*/")),
    keywords: PHP_KW,
    types: PHP_TYPES,
    constants: PHP_CONST,
    multiline_quotes: b"\"'",
    dollar_vars: true,
    cap_types: true,
    case_insensitive_kw: true,
    ..BASE
};
static BASH: Spec = Spec {
    line_comment: &["#"],
    keywords: BASH_KW,
    builtins: BASH_BUILTINS,
    quotes: b"\"'`",
    multiline_quotes: b"\"'`",
    dollar_vars: true,
    shell: true,
    ..BASE
};
static PYTHON: Spec = Spec {
    line_comment: &["#"],
    keywords: PY_KW,
    types: PY_TYPES,
    constants: PY_CONST,
    builtins: PY_BUILTINS,
    triple_quotes: b"\"'",
    decorators: true,
    cap_types: true,
    ..BASE
};
static JS: Spec = Spec {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: JS_KW,
    constants: JS_CONST,
    builtins: JS_BUILTINS,
    quotes: b"\"'`",
    multiline_quotes: b"`",
    cap_types: true,
    decorators: true,
    ..BASE
};
static TS: Spec = Spec { keywords: TS_KW, types: TS_TYPES, ..JS };
static JAVA: Spec = Spec {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: JAVA_KW,
    types: JAVA_TYPES,
    constants: JAVA_CONST,
    triple_quotes: b"\"",
    decorators: true,
    cap_types: true,
    ..BASE
};
static GO: Spec = Spec {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    keywords: GO_KW,
    types: GO_TYPES,
    constants: GO_CONST,
    builtins: GO_BUILTINS,
    quotes: b"\"'`",
    multiline_quotes: b"`",
    ..BASE
};
static YAML: Spec = Spec { line_comment: &["#"], ..BASE };
static HTML: Spec = Spec { block_comment: Some(("<!--", "-->")), ..BASE };
static CSS: Spec = Spec { block_comment: Some(("/*", "*/")), ..BASE };
static SQL: Spec = Spec {
    line_comment: &["--"],
    block_comment: Some(("/*", "*/")),
    keywords: SQL_KW,
    types: SQL_TYPES,
    constants: SQL_CONST,
    builtins: SQL_BUILTINS,
    quotes: b"'\"`",
    multiline_quotes: b"'",
    case_insensitive_kw: true,
    case_insensitive_words: true,
    ..BASE
};
static MARKDOWN: Spec = Spec { block_comment: Some(("<!--", "-->")), ..BASE };

impl Lang {
    pub fn id(self) -> &'static str {
        match self {
            Lang::Plain => "plain",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Rust => "rust",
            Lang::Php => "php",
            Lang::Bash => "bash",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Yaml => "yaml",
            Lang::Html => "html",
            Lang::Css => "css",
            Lang::Java => "java",
            Lang::Go => "go",
            Lang::Markdown => "markdown",
            Lang::Sql => "sql",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::Plain => "Testo semplice",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::Rust => "Rust",
            Lang::Php => "PHP",
            Lang::Bash => "Bash",
            Lang::Python => "Python",
            Lang::JavaScript => "JavaScript",
            Lang::TypeScript => "TypeScript",
            Lang::Yaml => "YAML",
            Lang::Html => "HTML",
            Lang::Css => "CSS",
            Lang::Java => "Java",
            Lang::Go => "Go",
            Lang::Markdown => "Markdown",
            Lang::Sql => "SQL",
        }
    }

    pub fn from_id(s: &str) -> Option<Lang> {
        ALL.iter().copied().find(|l| l.id() == s)
    }

    pub fn spec(self) -> &'static Spec {
        match self {
            Lang::Plain => &PLAIN,
            Lang::C => &C,
            Lang::Cpp => &CPP,
            Lang::Rust => &RUST,
            Lang::Php => &PHP,
            Lang::Bash => &BASH,
            Lang::Python => &PYTHON,
            Lang::JavaScript => &JS,
            Lang::TypeScript => &TS,
            Lang::Yaml => &YAML,
            Lang::Html => &HTML,
            Lang::Css => &CSS,
            Lang::Java => &JAVA,
            Lang::Go => &GO,
            Lang::Markdown => &MARKDOWN,
            Lang::Sql => &SQL,
        }
    }

    /// Text inserted by "toggle comment": (prefix, suffix).
    pub fn comment_tokens(self) -> Option<(&'static str, &'static str)> {
        let s = self.spec();
        if let Some(lc) = s.line_comment.first() {
            Some((lc, ""))
        } else {
            s.block_comment
        }
    }

    /// Language named by the info string of a Markdown code fence.
    pub fn from_fence(info: &str) -> Lang {
        let name = info.split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or("");
        match name.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "c" | "h" => Lang::C,
            "cpp" | "c++" | "cc" | "cxx" | "hpp" => Lang::Cpp,
            "rust" | "rs" => Lang::Rust,
            "php" => Lang::Php,
            "bash" | "sh" | "shell" | "zsh" | "console" | "shellsession" => Lang::Bash,
            "python" | "py" | "python3" => Lang::Python,
            "js" | "javascript" | "jsx" | "json" | "jsonc" | "node" => Lang::JavaScript,
            "ts" | "typescript" | "tsx" => Lang::TypeScript,
            "yaml" | "yml" => Lang::Yaml,
            "html" | "xml" | "svg" | "xhtml" => Lang::Html,
            "css" | "scss" | "less" => Lang::Css,
            "java" => Lang::Java,
            "go" | "golang" => Lang::Go,
            "md" | "markdown" => Lang::Markdown,
            "sql" | "pgsql" | "postgresql" | "postgres" | "psql" | "mysql" | "sqlite" | "plsql" | "plpgsql"
            | "tsql" | "mssql" | "mariadb" => Lang::Sql,
            _ => Lang::Plain,
        }
    }

    /// Whether a new block is opened by a trailing ':' (indentation based).
    pub fn colon_indents(self) -> bool {
        matches!(self, Lang::Python | Lang::Yaml)
    }

    /// Go code is conventionally indented with tabs.
    pub fn prefers_tabs(self) -> bool {
        matches!(self, Lang::Go)
    }
}

/// Detect the language from the file name and, failing that, the content.
pub fn detect(path: Option<&Path>, head: &str) -> Lang {
    if let Some(p) = path {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            ".bashrc" | ".bash_profile" | ".bash_logout" | ".profile" | ".zshrc" | ".zprofile"
            | "pkgbuild" | ".xinitrc" | "bashrc" => return Lang::Bash,
            "cmakelists.txt" => return Lang::Plain,
            _ => {}
        }
        if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
            let l = match ext {
                "c" | "h" => Lang::C,
                "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ino" | "tpp"
                | "ipp" => Lang::Cpp,
                "rs" => Lang::Rust,
                "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "php8" | "phps" => Lang::Php,
                "sh" | "bash" | "zsh" | "ksh" | "ebuild" | "install" => Lang::Bash,
                "py" | "pyw" | "pyi" => Lang::Python,
                "js" | "mjs" | "cjs" | "jsx" | "json" | "jsonc" => Lang::JavaScript,
                "ts" | "tsx" | "mts" | "cts" => Lang::TypeScript,
                "yml" | "yaml" => Lang::Yaml,
                "html" | "htm" | "xhtml" | "xml" | "svg" | "vue" => Lang::Html,
                "css" | "scss" | "less" => Lang::Css,
                "java" => Lang::Java,
                "go" => Lang::Go,
                "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx" => Lang::Markdown,
                "sql" | "pgsql" | "psql" | "mysql" | "ddl" | "dml" => Lang::Sql,
                _ => Lang::Plain,
            };
            if l != Lang::Plain {
                return l;
            }
        }
    }
    detect_content(head)
}

fn detect_content(head: &str) -> Lang {
    let first = head.lines().next().unwrap_or("").trim();
    if let Some(sb) = first.strip_prefix("#!") {
        // "#!/usr/bin/env python3" -> interpreter "python3"
        let mut parts = sb.split_whitespace();
        let mut interp = parts.next().unwrap_or("").rsplit('/').next().unwrap_or("");
        if interp == "env" {
            interp = parts.find(|p| !p.starts_with('-')).unwrap_or("");
        }
        return match interp.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
            "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash" => Lang::Bash,
            "python" => Lang::Python,
            "node" | "nodejs" | "deno" | "bun" => Lang::JavaScript,
            "php" => Lang::Php,
            _ => Lang::Plain,
        };
    }
    let lower = first.to_ascii_lowercase();
    if lower.starts_with("<?php") {
        Lang::Php
    } else if lower.starts_with("<!doctype html") || lower.starts_with("<html") || lower.starts_with("<?xml") {
        Lang::Html
    } else if first == "---" || first.starts_with("%YAML") {
        Lang::Yaml
    } else {
        Lang::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert_eq!(detect(Some(Path::new("/a/b.rs")), ""), Lang::Rust);
        assert_eq!(detect(Some(Path::new("x.HPP")), ""), Lang::Cpp);
        assert_eq!(detect(Some(Path::new("script")), "#!/usr/bin/env python3\n"), Lang::Python);
        assert_eq!(detect(Some(Path::new("run")), "#!/bin/bash\n"), Lang::Bash);
        assert_eq!(detect(None, "<?php echo 1;"), Lang::Php);
        assert_eq!(detect(Some(Path::new("PKGBUILD")), ""), Lang::Bash);
        assert_eq!(detect(Some(Path::new("notes.txt")), "hello"), Lang::Plain);
        assert_eq!(detect(Some(Path::new("README.md")), ""), Lang::Markdown);
        assert_eq!(Lang::from_fence("rust,ignore"), Lang::Rust);
        assert_eq!(Lang::from_fence("sh"), Lang::Bash);
        assert_eq!(detect(Some(Path::new("schema.SQL")), ""), Lang::Sql);
        assert_eq!(Lang::from_fence("postgresql"), Lang::Sql);
    }
}
