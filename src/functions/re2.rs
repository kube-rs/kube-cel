//! Go's `regexp/syntax` parser (go1.26.2), for what it accepts: the regular
//! expressions `regexp.Compile` takes, which the apiserver's `matches`,
//! `find` and `findAll` use.
//!
//! [`check`] is a port of `syntax.Parse` with the flags `regexp.Compile`
//! passes (`syntax.Perl`: Perl classes and flags, Unicode classes), keeping
//! only what decides whether the parse fails and with which error; it builds
//! no program. It leaves out the parser's resource limits (`expression too
//! large`, `expression nests too deeply`, a character-class rune budget),
//! which the `regex` crate's own limits are below; callers compile with it as
//! well.
//!
//! Its errors read as `regexp.Compile`'s: ``error parsing regexp: <code>:
//! `<expr>` ``.

/// A pattern Go's `regexp/syntax` refuses: its error code and the part of the
/// pattern it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Re2Error {
    code: &'static str,
    expr: String,
}

impl std::fmt::Display for Re2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error parsing regexp: {}: `{}`", self.code, self.expr)
    }
}

const INVALID_CHAR_CLASS_RANGE: &str = "invalid character class range";
const INVALID_ESCAPE: &str = "invalid escape sequence";
const INVALID_NAMED_CAPTURE: &str = "invalid named capture";
const INVALID_PERL_OP: &str = "invalid or unsupported Perl syntax";
const INVALID_REPEAT_OP: &str = "invalid nested repetition operator";
const INVALID_REPEAT_SIZE: &str = "invalid repeat count";
const MISSING_BRACKET: &str = "missing closing ]";
const MISSING_PAREN: &str = "missing closing )";
const MISSING_REPEAT_ARGUMENT: &str = "missing argument to repetition operator";
const TRAILING_BACKSLASH: &str = "trailing backslash at end of expression";
const UNEXPECTED_PAREN: &str = "unexpected )";

fn error(code: &'static str, expr: &str) -> Re2Error {
    Re2Error {
        code,
        expr: expr.to_owned(),
    }
}

/// The part of `s` before its suffix `rest`, as Go's `s[:len(s)-len(rest)]`.
fn prefix<'a>(s: &'a str, rest: &str) -> &'a str {
    &s[..s.len() - rest.len()]
}

/// Go's `nextRune` on a `&str`, which is always UTF-8: the first rune and the
/// rest. On an empty string, Go's `utf8.DecodeRuneInString` gives U+FFFD and
/// consumes nothing.
fn next_rune(s: &str) -> (char, &str) {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => (c, chars.as_str()),
        None => ('\u{FFFD}', s),
    }
}

/// The names `\p{..}` takes after Go's `canonicalName`: `Any`, `Assigned`,
/// `Ascii`, `Lc`, the general categories, the scripts and category aliases
/// whose canonical form is a key of Go's `unicode` tables (Unicode 15.0.0). A
/// script whose name has an underscore (`Old_Italic`) is not among them: Go
/// looks up its canonical form (`Olditalic`), which is no key.
const UNICODE_CLASSES: &[&str] = &[
    "Adlam",
    "Ahom",
    "Any",
    "Arabic",
    "Armenian",
    "Ascii",
    "Assigned",
    "Avestan",
    "Balinese",
    "Bamum",
    "Batak",
    "Bengali",
    "Bhaiksuki",
    "Bopomofo",
    "Brahmi",
    "Braille",
    "Buginese",
    "Buhid",
    "C",
    "Carian",
    "Casedletter",
    "Cc",
    "Cf",
    "Chakma",
    "Cham",
    "Cherokee",
    "Chorasmian",
    "Closepunctuation",
    "Cn",
    "Cntrl",
    "Co",
    "Combiningmark",
    "Common",
    "Connectorpunctuation",
    "Control",
    "Coptic",
    "Cs",
    "Cuneiform",
    "Currencysymbol",
    "Cypriot",
    "Cyrillic",
    "Dashpunctuation",
    "Decimalnumber",
    "Deseret",
    "Devanagari",
    "Digit",
    "Dogra",
    "Duployan",
    "Elbasan",
    "Elymaic",
    "Enclosingmark",
    "Ethiopic",
    "Finalpunctuation",
    "Format",
    "Georgian",
    "Glagolitic",
    "Gothic",
    "Grantha",
    "Greek",
    "Gujarati",
    "Gurmukhi",
    "Han",
    "Hangul",
    "Hanunoo",
    "Hatran",
    "Hebrew",
    "Hiragana",
    "Inherited",
    "Initialpunctuation",
    "Javanese",
    "Kaithi",
    "Kannada",
    "Katakana",
    "Kawi",
    "Kharoshthi",
    "Khmer",
    "Khojki",
    "Khudawadi",
    "L",
    "Lao",
    "Latin",
    "Lc",
    "Lepcha",
    "Letter",
    "Letternumber",
    "Limbu",
    "Lineseparator",
    "Lisu",
    "Ll",
    "Lm",
    "Lo",
    "Lowercaseletter",
    "Lt",
    "Lu",
    "Lycian",
    "Lydian",
    "M",
    "Mahajani",
    "Makasar",
    "Malayalam",
    "Mandaic",
    "Manichaean",
    "Marchen",
    "Mark",
    "Mathsymbol",
    "Mc",
    "Me",
    "Medefaidrin",
    "Miao",
    "Mn",
    "Modi",
    "Modifierletter",
    "Modifiersymbol",
    "Mongolian",
    "Mro",
    "Multani",
    "Myanmar",
    "N",
    "Nabataean",
    "Nandinagari",
    "Nd",
    "Newa",
    "Nko",
    "Nl",
    "No",
    "Nonspacingmark",
    "Number",
    "Nushu",
    "Ogham",
    "Openpunctuation",
    "Oriya",
    "Osage",
    "Osmanya",
    "Other",
    "Otherletter",
    "Othernumber",
    "Otherpunctuation",
    "Othersymbol",
    "P",
    "Palmyrene",
    "Paragraphseparator",
    "Pc",
    "Pd",
    "Pe",
    "Pf",
    "Phoenician",
    "Pi",
    "Po",
    "Privateuse",
    "Ps",
    "Punct",
    "Punctuation",
    "Rejang",
    "Runic",
    "S",
    "Samaritan",
    "Saurashtra",
    "Sc",
    "Separator",
    "Sharada",
    "Shavian",
    "Siddham",
    "Sinhala",
    "Sk",
    "Sm",
    "So",
    "Sogdian",
    "Soyombo",
    "Spaceseparator",
    "Spacingmark",
    "Sundanese",
    "Surrogate",
    "Symbol",
    "Syriac",
    "Tagalog",
    "Tagbanwa",
    "Takri",
    "Tamil",
    "Tangsa",
    "Tangut",
    "Telugu",
    "Thaana",
    "Thai",
    "Tibetan",
    "Tifinagh",
    "Tirhuta",
    "Titlecaseletter",
    "Toto",
    "Ugaritic",
    "Unassigned",
    "Uppercaseletter",
    "Vai",
    "Vithkuqi",
    "Wancho",
    "Yezidi",
    "Yi",
    "Z",
    "Zl",
    "Zp",
    "Zs",
];

/// Go's `canonicalName`: underscores, hyphens and spaces dropped, the first
/// other byte uppercased and the rest lowercased (ASCII letters only).
fn canonical_name(name: &str) -> String {
    let mut out = Vec::with_capacity(name.len());
    let mut first = true;
    for &c in name.as_bytes() {
        match c {
            b'_' | b'-' | b' ' => {}
            _ if first => {
                out.push(c.to_ascii_uppercase());
                first = false;
            }
            _ => out.push(c.to_ascii_lowercase()),
        }
    }
    String::from_utf8(out).expect("only ASCII bytes are changed")
}

const POSIX_CLASSES: &[&str] = &[
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "word", "xdigit",
];

/// A node of the parse: what `repeatIsValid` walks, and the parser's two
/// pseudo-operators.
#[derive(Debug)]
enum Node {
    /// `(`, capturing or not.
    LeftParen,
    /// `|`.
    VerticalBar,
    /// Anything else: a repetition carries its bounds, every node its
    /// sub-expressions.
    Expr {
        repeat: Option<(i32, i32)>,
        subs: Vec<Node>,
    },
}

impl Node {
    fn leaf() -> Node {
        Node::Expr {
            repeat: None,
            subs: Vec::new(),
        }
    }

    fn is_pseudo(&self) -> bool {
        !matches!(self, Node::Expr { .. })
    }
}

/// Go's `repeatIsValid`: the repetition and those inside it make at most `n`
/// copies of the innermost thing.
fn repeat_is_valid(node: &Node, mut n: i32) -> bool {
    let Node::Expr { repeat, subs } = node else {
        return true;
    };
    if let Some((min, max)) = *repeat {
        let mut m = max;
        if m == 0 {
            return true;
        }
        if m < 0 {
            m = min;
        }
        if m > n {
            return false;
        }
        if m > 0 {
            n /= m;
        }
    }
    subs.iter().all(|sub| repeat_is_valid(sub, n))
}

#[derive(Default)]
struct Parser {
    stack: Vec<Node>,
}

impl Parser {
    fn push(&mut self, node: Node) {
        self.stack.push(node);
    }

    /// Go's `concat`: the expressions above the topmost pseudo-operator
    /// become one.
    fn concat(&mut self) {
        let i = self.stack.iter().rposition(Node::is_pseudo).map_or(0, |i| i + 1);
        let subs = self.stack.split_off(i);
        self.push(Node::Expr { repeat: None, subs });
    }

    /// Go's `alternate`, the same over `|`.
    fn alternate(&mut self) {
        self.concat();
    }

    /// Go's `swapVerticalBar`: a `|` below the top moves to the top (Go merges
    /// two character classes around it instead, which leaves the same shape).
    fn swap_vertical_bar(&mut self) -> bool {
        let n = self.stack.len();
        if n >= 2 && matches!(self.stack[n - 2], Node::VerticalBar) {
            self.stack.swap(n - 2, n - 1);
            return true;
        }
        false
    }

    fn parse_vertical_bar(&mut self) {
        self.concat();
        if !self.swap_vertical_bar() {
            self.push(Node::VerticalBar);
        }
    }

    fn parse_right_paren(&mut self, whole: &str) -> Result<(), Re2Error> {
        self.concat();
        if self.swap_vertical_bar() {
            self.stack.pop();
        }
        self.alternate();
        let n = self.stack.len();
        if n < 2 || !matches!(self.stack[n - 2], Node::LeftParen) {
            return Err(error(UNEXPECTED_PAREN, whole));
        }
        let inner = self.stack.pop().expect("n >= 2");
        self.stack.pop();
        self.push(Node::Expr {
            repeat: None,
            subs: vec![inner],
        });
        Ok(())
    }

    /// Go's `repeat`: `op` (`*`, `+`, `?` or `{..}`, with `bounds` for the
    /// last) applied to the top of the stack. `before` starts at the operator,
    /// `after` follows it; returns `after` past a non-greedy `?`.
    fn repeat<'a>(
        &mut self,
        bounds: Option<(i32, i32)>,
        before: &str,
        mut after: &'a str,
        last_repeat: &str,
    ) -> Result<&'a str, Re2Error> {
        if let Some(rest) = after.strip_prefix('?') {
            after = rest;
        }
        if !last_repeat.is_empty() {
            return Err(error(INVALID_REPEAT_OP, prefix(last_repeat, after)));
        }
        let Some(sub) = self.stack.pop() else {
            return Err(error(MISSING_REPEAT_ARGUMENT, prefix(before, after)));
        };
        if sub.is_pseudo() {
            return Err(error(MISSING_REPEAT_ARGUMENT, prefix(before, after)));
        }
        let node = Node::Expr {
            repeat: bounds,
            subs: vec![sub],
        };
        if let Some((min, max)) = bounds
            && (min >= 2 || max >= 2)
            && !repeat_is_valid(&node, 1000)
        {
            return Err(error(INVALID_REPEAT_SIZE, prefix(before, after)));
        }
        self.push(node);
        Ok(after)
    }
}

/// Whether Go's `regexp.Compile` parses `pattern`, and if not its error.
pub(crate) fn check(pattern: &str) -> Result<(), Re2Error> {
    let mut p = Parser::default();
    let mut t = pattern;
    let mut last_repeat: &str = "";
    while !t.is_empty() {
        let mut repeat: &str = "";
        let b = t.as_bytes();
        match b[0] {
            b'(' => {
                if b.get(1) == Some(&b'?') {
                    t = parse_perl_flags(&mut p, t)?;
                } else {
                    p.push(Node::LeftParen);
                    t = &t[1..];
                }
            }
            b'|' => {
                p.parse_vertical_bar();
                t = &t[1..];
            }
            b')' => {
                p.parse_right_paren(pattern)?;
                t = &t[1..];
            }
            b'^' | b'$' | b'.' => {
                p.push(Node::leaf());
                t = &t[1..];
            }
            b'[' => t = parse_class(&mut p, t)?,
            b'*' | b'+' | b'?' => {
                let before = t;
                t = p.repeat(None, before, &t[1..], last_repeat)?;
                repeat = before;
            }
            b'{' => {
                let before = t;
                match parse_repeat(t) {
                    None => {
                        p.push(Node::leaf());
                        t = &t[1..];
                    }
                    Some((min, max, after)) => {
                        if !(0..=1000).contains(&min) || max > 1000 || (max >= 0 && min > max) {
                            return Err(error(INVALID_REPEAT_SIZE, prefix(before, after)));
                        }
                        t = p.repeat(Some((min, max)), before, after, last_repeat)?;
                        repeat = before;
                    }
                }
            }
            b'\\' => t = parse_backslash(&mut p, t)?,
            _ => {
                p.push(Node::leaf());
                t = next_rune(t).1;
            }
        }
        last_repeat = repeat;
    }
    p.concat();
    if p.swap_vertical_bar() {
        p.stack.pop();
    }
    p.alternate();
    if p.stack.len() != 1 {
        return Err(error(MISSING_PAREN, pattern));
    }
    Ok(())
}

/// A `\` outside a class: `\A`, `\b`, `\B`, `\z`, `\Q..\E`, a Unicode or Perl
/// class, or a single-character escape.
fn parse_backslash<'a>(p: &mut Parser, t: &'a str) -> Result<&'a str, Re2Error> {
    match t.as_bytes().get(1) {
        Some(b'A' | b'b' | b'B' | b'z') => {
            p.push(Node::leaf());
            return Ok(&t[2..]);
        }
        // Any byte: not supported.
        Some(b'C') => return Err(error(INVALID_ESCAPE, &t[..2])),
        Some(b'Q') => {
            // `\Q ... \E`: the ... is always literals.
            let (lit, rest) = t[2..].split_once("\\E").unwrap_or((&t[2..], ""));
            for _ in lit.chars() {
                p.push(Node::leaf());
            }
            return Ok(rest);
        }
        _ => {}
    }
    if let Some(rest) = parse_unicode_class(t)? {
        p.push(Node::leaf());
        return Ok(rest);
    }
    if let Some(rest) = parse_perl_class_escape(t) {
        p.push(Node::leaf());
        return Ok(rest);
    }
    let (_, rest) = parse_escape(t)?;
    p.push(Node::leaf());
    Ok(rest)
}

/// Go's `parseRepeat`: `{min}`, `{min,}` (max -1) or `{min,max}`; `None` if
/// `s` is not of that form (the `{` is a literal then). A number too big
/// gives min -1.
fn parse_repeat(s: &str) -> Option<(i32, i32, &str)> {
    let s = s.strip_prefix('{')?;
    let (mut min, s) = parse_int(s)?;
    let max;
    let s = if let Some(s) = s.strip_prefix(',') {
        if s.is_empty() {
            return None;
        }
        if s.starts_with('}') {
            max = -1;
            s
        } else {
            let (m, s) = parse_int(s)?;
            max = m;
            if m < 0 {
                min = -1;
            }
            s
        }
    } else {
        if s.is_empty() {
            return None;
        }
        max = min;
        s
    };
    let rest = s.strip_prefix('}')?;
    Some((min, max, rest))
}

/// Go's `parseInt`: decimal digits without a leading zero; -1 past 10^8.
fn parse_int(s: &str) -> Option<(i32, &str)> {
    let b = s.as_bytes();
    if b.first().is_none_or(|c| !c.is_ascii_digit()) {
        return None;
    }
    if b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit() {
        return None;
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut n: i32 = 0;
    for &c in &b[..digits] {
        if n >= 100_000_000 {
            n = -1;
            break;
        }
        n = n * 10 + i32::from(c - b'0');
    }
    Some((n, &s[digits..]))
}

/// Go's `parsePerlFlags`: a named capture, a non-capturing group or a flag
/// change, `s` starting with `(?`.
fn parse_perl_flags<'a>(p: &mut Parser, s: &'a str) -> Result<&'a str, Re2Error> {
    let b = s.as_bytes();
    let starts_with_p = b.len() > 4 && b[2] == b'P' && b[3] == b'<';
    let starts_with_name = b.len() > 3 && b[2] == b'<';
    if starts_with_p || starts_with_name {
        let start = if starts_with_name { 3 } else { 4 };
        let Some(end) = s.find('>') else {
            return Err(error(INVALID_NAMED_CAPTURE, s));
        };
        let name = &s[start..end];
        if name.is_empty() || !name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) {
            return Err(error(INVALID_NAMED_CAPTURE, &s[..=end]));
        }
        p.push(Node::LeftParen);
        return Ok(&s[end + 1..]);
    }

    let mut t = &s[2..];
    let mut negated = false;
    let mut saw_flag = false;
    while !t.is_empty() {
        let (c, rest) = next_rune(t);
        t = rest;
        match c {
            'i' | 'm' | 's' | 'U' => saw_flag = true,
            '-' if !negated => {
                negated = true;
                saw_flag = false;
            }
            ':' | ')' if !negated || saw_flag => {
                if c == ':' {
                    p.push(Node::LeftParen);
                }
                return Ok(t);
            }
            _ => break,
        }
    }
    Err(error(INVALID_PERL_OP, prefix(s, t)))
}

/// Go's `parseEscape`: the rune a single-character escape at the start of
/// `s` stands for, and the rest.
fn parse_escape(s: &str) -> Result<(u32, &str), Re2Error> {
    let t = &s[1..];
    if t.is_empty() {
        return Err(error(TRAILING_BACKSLASH, ""));
    }
    let (c, mut t) = next_rune(t);
    match c {
        '1'..='7' if !t.as_bytes().first().is_some_and(|d| (b'0'..=b'7').contains(d)) => {}
        '0'..='7' => {
            // Up to three octal digits; one is c.
            let mut r = u32::from(c) - u32::from('0');
            for _ in 1..3 {
                match t.as_bytes().first() {
                    Some(d @ b'0'..=b'7') => {
                        r = r * 8 + u32::from(d - b'0');
                        t = &t[1..];
                    }
                    _ => break,
                }
            }
            return Ok((r, t));
        }
        'x' if !t.is_empty() => {
            let (c, rest) = next_rune(t);
            t = rest;
            if c == '{' {
                // Any number of hex digits in braces, at least one.
                let mut nhex = 0;
                let mut r: u32 = 0;
                loop {
                    if t.is_empty() {
                        return Err(error(INVALID_ESCAPE, prefix(s, t)));
                    }
                    let (c, rest) = next_rune(t);
                    t = rest;
                    if c == '}' {
                        break;
                    }
                    let Some(v) = c.to_digit(16) else {
                        return Err(error(INVALID_ESCAPE, prefix(s, t)));
                    };
                    r = r * 16 + v;
                    if r > 0x10FFFF {
                        return Err(error(INVALID_ESCAPE, prefix(s, t)));
                    }
                    nhex += 1;
                }
                if nhex == 0 {
                    return Err(error(INVALID_ESCAPE, prefix(s, t)));
                }
                return Ok((r, t));
            }
            // Two hex digits.
            let x = c.to_digit(16);
            let (c, rest) = next_rune(t);
            t = rest;
            if let (Some(x), Some(y)) = (x, c.to_digit(16)) {
                return Ok((x * 16 + y, t));
            }
        }
        'a' => return Ok((0x07, t)),
        'f' => return Ok((0x0C, t)),
        'n' => return Ok((0x0A, t)),
        'r' => return Ok((0x0D, t)),
        't' => return Ok((0x09, t)),
        'v' => return Ok((0x0B, t)),
        // Escaped non-word characters are always themselves.
        c if c.is_ascii() && !c.is_ascii_alphanumeric() => return Ok((u32::from(c), t)),
        _ => {}
    }
    Err(error(INVALID_ESCAPE, prefix(s, t)))
}

/// Go's `parsePerlClassEscape`: `\d`, `\D`, `\s`, `\S`, `\w`, `\W`.
fn parse_perl_class_escape(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0] == b'\\' && matches!(b[1], b'd' | b'D' | b's' | b'S' | b'w' | b'W'))
        .then(|| &s[2..])
}

/// Go's `parseUnicodeClass`: a `\pN`, `\p{Name}` or `\P..` at the start of
/// `s`; `None` if `s` starts with none.
fn parse_unicode_class(s: &str) -> Result<Option<&str>, Re2Error> {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'\\' || (b[1] != b'p' && b[1] != b'P') {
        return Ok(None);
    }
    let (c, t) = next_rune(&s[2..]);
    let (seq, name, rest) = if c != '{' {
        // A one-letter name.
        let seq = prefix(s, t);
        (seq, &seq[2..], t)
    } else {
        let Some(end) = s.find('}') else {
            return Err(error(INVALID_CHAR_CLASS_RANGE, s));
        };
        (&s[..=end], &s[3..end], &s[end + 1..])
    };
    // A leading `^` negates: `\p{^Han}` is `\P{Han}`.
    let name = name.strip_prefix('^').unwrap_or(name);
    if UNICODE_CLASSES
        .binary_search(&canonical_name(name).as_str())
        .is_err()
    {
        return Err(error(INVALID_CHAR_CLASS_RANGE, seq));
    }
    Ok(Some(rest))
}

/// Go's `parseNamedClass`: a POSIX class `[:alnum:]` (or `[:^alnum:]`) at the
/// start of `s`; `None` if `s` starts with no `[:..:]`.
fn parse_named_class(s: &str) -> Result<Option<&str>, Re2Error> {
    if !s.starts_with("[:") {
        return Ok(None);
    }
    let Some(i) = s[2..].find(":]") else {
        return Ok(None);
    };
    let i = i + 2;
    let (name, rest) = (&s[..i + 2], &s[i + 2..]);
    let inner = &name[2..name.len() - 2];
    let inner = inner.strip_prefix('^').unwrap_or(inner);
    if !POSIX_CLASSES.contains(&inner) {
        return Err(error(INVALID_CHAR_CLASS_RANGE, name));
    }
    Ok(Some(rest))
}

/// Go's `parseClassChar`: one character of a class, escaped or not.
fn parse_class_char<'a>(s: &'a str, whole: &str) -> Result<(u32, &'a str), Re2Error> {
    if s.is_empty() {
        return Err(error(MISSING_BRACKET, whole));
    }
    if s.starts_with('\\') {
        return parse_escape(s);
    }
    let (c, rest) = next_rune(s);
    Ok((u32::from(c), rest))
}

/// Go's `parseClass`: a `[..]` class at the start of `s`.
fn parse_class<'a>(p: &mut Parser, s: &'a str) -> Result<&'a str, Re2Error> {
    let mut t = &s[1..];
    if let Some(rest) = t.strip_prefix('^') {
        t = rest;
    }
    // `]` (and `-`) may come first.
    let mut first = true;
    while t.is_empty() || !t.starts_with(']') || first {
        first = false;
        if t.len() > 2
            && let Some(rest) = parse_named_class(t)?
        {
            t = rest;
            continue;
        }
        if let Some(rest) = parse_unicode_class(t)? {
            t = rest;
            continue;
        }
        if let Some(rest) = parse_perl_class_escape(t) {
            t = rest;
            continue;
        }
        let range = t;
        let (lo, rest) = parse_class_char(t, s)?;
        t = rest;
        // `[a-]` is `a` or `-`: a range only before something other than `]`.
        let b = t.as_bytes();
        if b.len() >= 2 && b[0] == b'-' && b[1] != b']' {
            let (hi, rest) = parse_class_char(&t[1..], s)?;
            t = rest;
            if hi < lo {
                return Err(error(INVALID_CHAR_CLASS_RANGE, prefix(range, t)));
            }
        }
    }
    p.push(Node::leaf());
    Ok(&t[1..])
}

#[cfg(test)]
mod tests {
    /// Go's `regexp.Compile` on each pattern (go1.26.2): `OK` or its error.
    const CASES: &[(&str, &str)] = &[
        (
            r#"a**"#,
            r#"error parsing regexp: invalid nested repetition operator: `**`"#,
        ),
        (
            r#"a*+"#,
            r#"error parsing regexp: invalid nested repetition operator: `*+`"#,
        ),
        (
            r#"a{2}{3}"#,
            r#"error parsing regexp: invalid nested repetition operator: `{2}{3}`"#,
        ),
        (r#"a*?"#, r#"OK"#),
        (
            r#"a{1001}"#,
            r#"error parsing regexp: invalid repeat count: `{1001}`"#,
        ),
        (
            r#"(a{10}){101}"#,
            r#"error parsing regexp: invalid repeat count: `{101}`"#,
        ),
        (r#"(?:a{10}){100}"#, r#"OK"#),
        (r#"a{,3}"#, r#"OK"#),
        (r#"a{01}"#, r#"OK"#),
        (
            r#"x{2,1}"#,
            r#"error parsing regexp: invalid repeat count: `{2,1}`"#,
        ),
        (
            r#"\u0041"#,
            r#"error parsing regexp: invalid escape sequence: `\u`"#,
        ),
        (r#"\x41\x{1F600}"#, r#"OK"#),
        (
            r#"\x{110000}"#,
            r#"error parsing regexp: invalid escape sequence: `\x{110000`"#,
        ),
        (
            r#"\x{}"#,
            r#"error parsing regexp: invalid escape sequence: `\x{}`"#,
        ),
        (
            r#"\x4"#,
            r#"error parsing regexp: invalid escape sequence: `\x4`"#,
        ),
        (r#"\0\012\123"#, r#"OK"#),
        (r#"\1"#, r#"error parsing regexp: invalid escape sequence: `\1`"#),
        (r#"\8"#, r#"error parsing regexp: invalid escape sequence: `\8`"#),
        (r#"\Q(a\E"#, r#"OK"#),
        (r#"\C"#, r#"error parsing regexp: invalid escape sequence: `\C`"#),
        (r#"\z\A\b\B"#, r#"OK"#),
        (
            r#"(?x)a"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?x`"#,
        ),
        (r#"(?i)a(?-i:b)(?im-sU)"#, r#"OK"#),
        (r#"(?)"#, r#"OK"#),
        (
            r#"(?-)"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?-)`"#,
        ),
        (
            r#"(?i-)"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?i-)`"#,
        ),
        (r#"(?P<name>a)(?<n2>b)"#, r#"OK"#),
        (
            r#"(?P<a.b>x)"#,
            r#"error parsing regexp: invalid named capture: `(?P<a.b>`"#,
        ),
        (
            r#"(?<>x)"#,
            r#"error parsing regexp: invalid named capture: `(?<>`"#,
        ),
        (
            r#"(?'n'x)"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?'`"#,
        ),
        (
            r#"(?=a)"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?=`"#,
        ),
        (
            r#"(?P=n)"#,
            r#"error parsing regexp: invalid or unsupported Perl syntax: `(?P`"#,
        ),
        (r#"(a"#, r#"error parsing regexp: missing closing ): `(a`"#),
        (r#"a)"#, r#"error parsing regexp: unexpected ): `a)`"#),
        (
            r#"*a"#,
            r#"error parsing regexp: missing argument to repetition operator: `*`"#,
        ),
        (
            r#"(|*)"#,
            r#"error parsing regexp: missing argument to repetition operator: `*`"#,
        ),
        (
            r#"a|*"#,
            r#"error parsing regexp: missing argument to repetition operator: `*`"#,
        ),
        (r#"^*"#, r#"OK"#),
        (r#"[a-z0-9_-]"#, r#"OK"#),
        (
            r#"[z-a]"#,
            r#"error parsing regexp: invalid character class range: `z-a`"#,
        ),
        (r#"[[:alpha:][:^digit:]]"#, r#"OK"#),
        (
            r#"[[:foo:]]"#,
            r#"error parsing regexp: invalid character class range: `[:foo:]`"#,
        ),
        (
            r#"[a--b]"#,
            r#"error parsing regexp: invalid character class range: `a--`"#,
        ),
        (r#"[a&&b]"#, r#"OK"#),
        (r#"[a[b]]"#, r#"OK"#),
        (r#"[]a]"#, r#"OK"#),
        (r#"[^]"#, r#"error parsing regexp: missing closing ]: `[^]`"#),
        (r#"[a"#, r#"error parsing regexp: missing closing ]: `[a`"#),
        (
            r#"\pL\p{Greek}\p{greek}\p{Lu}\p{Uppercase_Letter}\p{^L}\PN"#,
            r#"OK"#,
        ),
        (
            r#"\p{Old_Italic}"#,
            r#"error parsing regexp: invalid character class range: `\p{Old_Italic}`"#,
        ),
        (
            r#"\p{Emoji}"#,
            r#"error parsing regexp: invalid character class range: `\p{Emoji}`"#,
        ),
        (
            r#"\p{IsGreek}"#,
            r#"error parsing regexp: invalid character class range: `\p{IsGreek}`"#,
        ),
        (r#"\p{Ascii}\p{Assigned}\p{Any}\p{LC}"#, r#"OK"#),
        (
            r#"\p{"#,
            r#"error parsing regexp: invalid character class range: `\p{`"#,
        ),
        (
            r#"\p"#,
            r#"error parsing regexp: invalid character class range: `\p`"#,
        ),
        (
            r#"\"#,
            r#"error parsing regexp: trailing backslash at end of expression: ``"#,
        ),
        (r#"\é"#, r#"error parsing regexp: invalid escape sequence: `\é`"#),
        (r#"\_\-\ \!"#, r#"OK"#),
    ];

    #[test]
    fn verdicts_and_messages_are_go_s() {
        for (pattern, want) in CASES {
            let got = match super::check(pattern) {
                Ok(()) => "OK".to_owned(),
                Err(e) => e.to_string(),
            };
            assert_eq!(&got, want, "{pattern}");
        }
    }

    #[test]
    fn unicode_classes_are_sorted_for_the_binary_search() {
        assert!(super::UNICODE_CLASSES.windows(2).all(|w| w[0] < w[1]));
    }
}
