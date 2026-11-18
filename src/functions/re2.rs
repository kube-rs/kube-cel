//! Go's `regexp/syntax` parser (go1.26.2): the regular expressions
//! `regexp.Compile` takes, which the apiserver's `matches`, `find` and
//! `findAll` use, and what they match.
//!
//! [`translate`] is a port of `syntax.Parse` with the flags `regexp.Compile`
//! passes (`syntax.Perl`: Perl classes and flags, Unicode classes): it fails
//! where Go fails, with Go's error, and otherwise writes the pattern in the
//! `regex` crate's syntax so that it matches what Go matches. The two
//! syntaxes share most of their spelling but not its meaning: Go's `\d`,
//! `\w`, `\s` and `\b` are ASCII (its `\s` without `\v`), its `\<` and
//! `\>` are literals, `&&`, `--`, `~~` and `[` inside a class are literals;
//! the `regex` crate has them Unicode, as word boundaries and as class
//! operations. What the translation cannot carry over is Unicode itself: Go's
//! tables are Unicode 15.0.0, the crate's newer (see `go_unicode`).
//!
//! It leaves out the parser's resource limits (`expression too large`,
//! `expression nests too deeply`, a character-class rune budget), which the
//! `regex` crate's own limits are below; callers compile its output with it.
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

/// The names `\p{..}` takes after Go's `canonicalName`, each with the name of
/// the same class in the `regex` crate: `Any`, `Assigned`, `Ascii`, `Lc`, the
/// general categories, the scripts and the category aliases whose canonical
/// form is a key of Go's `unicode` tables (Unicode 15.0.0). A script whose
/// name has an underscore (`Old_Italic`) is not among them: Go looks up its
/// canonical form (`Olditalic`), which is no key.
const UNICODE_CLASSES: &[(&str, &str)] = &[
    ("Adlam", "Adlam"),
    ("Ahom", "Ahom"),
    ("Any", "Any"),
    ("Arabic", "Arabic"),
    ("Armenian", "Armenian"),
    ("Ascii", "ASCII"),
    ("Assigned", "Assigned"),
    ("Avestan", "Avestan"),
    ("Balinese", "Balinese"),
    ("Bamum", "Bamum"),
    ("Batak", "Batak"),
    ("Bengali", "Bengali"),
    ("Bhaiksuki", "Bhaiksuki"),
    ("Bopomofo", "Bopomofo"),
    ("Brahmi", "Brahmi"),
    ("Braille", "Braille"),
    ("Buginese", "Buginese"),
    ("Buhid", "Buhid"),
    ("C", "C"),
    ("Carian", "Carian"),
    ("Casedletter", "LC"),
    ("Cc", "Cc"),
    ("Cf", "Cf"),
    ("Chakma", "Chakma"),
    ("Cham", "Cham"),
    ("Cherokee", "Cherokee"),
    ("Chorasmian", "Chorasmian"),
    ("Closepunctuation", "Pe"),
    ("Cn", "Cn"),
    ("Cntrl", "Cc"),
    ("Co", "Co"),
    ("Combiningmark", "M"),
    ("Common", "Common"),
    ("Connectorpunctuation", "Pc"),
    ("Control", "Cc"),
    ("Coptic", "Coptic"),
    ("Cs", "Cs"),
    ("Cuneiform", "Cuneiform"),
    ("Currencysymbol", "Sc"),
    ("Cypriot", "Cypriot"),
    ("Cyrillic", "Cyrillic"),
    ("Dashpunctuation", "Pd"),
    ("Decimalnumber", "Nd"),
    ("Deseret", "Deseret"),
    ("Devanagari", "Devanagari"),
    ("Digit", "Nd"),
    ("Dogra", "Dogra"),
    ("Duployan", "Duployan"),
    ("Elbasan", "Elbasan"),
    ("Elymaic", "Elymaic"),
    ("Enclosingmark", "Me"),
    ("Ethiopic", "Ethiopic"),
    ("Finalpunctuation", "Pf"),
    ("Format", "Cf"),
    ("Georgian", "Georgian"),
    ("Glagolitic", "Glagolitic"),
    ("Gothic", "Gothic"),
    ("Grantha", "Grantha"),
    ("Greek", "Greek"),
    ("Gujarati", "Gujarati"),
    ("Gurmukhi", "Gurmukhi"),
    ("Han", "Han"),
    ("Hangul", "Hangul"),
    ("Hanunoo", "Hanunoo"),
    ("Hatran", "Hatran"),
    ("Hebrew", "Hebrew"),
    ("Hiragana", "Hiragana"),
    ("Inherited", "Inherited"),
    ("Initialpunctuation", "Pi"),
    ("Javanese", "Javanese"),
    ("Kaithi", "Kaithi"),
    ("Kannada", "Kannada"),
    ("Katakana", "Katakana"),
    ("Kawi", "Kawi"),
    ("Kharoshthi", "Kharoshthi"),
    ("Khmer", "Khmer"),
    ("Khojki", "Khojki"),
    ("Khudawadi", "Khudawadi"),
    ("L", "L"),
    ("Lao", "Lao"),
    ("Latin", "Latin"),
    ("Lc", "LC"),
    ("Lepcha", "Lepcha"),
    ("Letter", "L"),
    ("Letternumber", "Nl"),
    ("Limbu", "Limbu"),
    ("Lineseparator", "Zl"),
    ("Lisu", "Lisu"),
    ("Ll", "Ll"),
    ("Lm", "Lm"),
    ("Lo", "Lo"),
    ("Lowercaseletter", "Ll"),
    ("Lt", "Lt"),
    ("Lu", "Lu"),
    ("Lycian", "Lycian"),
    ("Lydian", "Lydian"),
    ("M", "M"),
    ("Mahajani", "Mahajani"),
    ("Makasar", "Makasar"),
    ("Malayalam", "Malayalam"),
    ("Mandaic", "Mandaic"),
    ("Manichaean", "Manichaean"),
    ("Marchen", "Marchen"),
    ("Mark", "M"),
    ("Mathsymbol", "Sm"),
    ("Mc", "Mc"),
    ("Me", "Me"),
    ("Medefaidrin", "Medefaidrin"),
    ("Miao", "Miao"),
    ("Mn", "Mn"),
    ("Modi", "Modi"),
    ("Modifierletter", "Lm"),
    ("Modifiersymbol", "Sk"),
    ("Mongolian", "Mongolian"),
    ("Mro", "Mro"),
    ("Multani", "Multani"),
    ("Myanmar", "Myanmar"),
    ("N", "N"),
    ("Nabataean", "Nabataean"),
    ("Nandinagari", "Nandinagari"),
    ("Nd", "Nd"),
    ("Newa", "Newa"),
    ("Nko", "Nko"),
    ("Nl", "Nl"),
    ("No", "No"),
    ("Nonspacingmark", "Mn"),
    ("Number", "N"),
    ("Nushu", "Nushu"),
    ("Ogham", "Ogham"),
    ("Openpunctuation", "Ps"),
    ("Oriya", "Oriya"),
    ("Osage", "Osage"),
    ("Osmanya", "Osmanya"),
    ("Other", "C"),
    ("Otherletter", "Lo"),
    ("Othernumber", "No"),
    ("Otherpunctuation", "Po"),
    ("Othersymbol", "So"),
    ("P", "P"),
    ("Palmyrene", "Palmyrene"),
    ("Paragraphseparator", "Zp"),
    ("Pc", "Pc"),
    ("Pd", "Pd"),
    ("Pe", "Pe"),
    ("Pf", "Pf"),
    ("Phoenician", "Phoenician"),
    ("Pi", "Pi"),
    ("Po", "Po"),
    ("Privateuse", "Co"),
    ("Ps", "Ps"),
    ("Punct", "P"),
    ("Punctuation", "P"),
    ("Rejang", "Rejang"),
    ("Runic", "Runic"),
    ("S", "S"),
    ("Samaritan", "Samaritan"),
    ("Saurashtra", "Saurashtra"),
    ("Sc", "Sc"),
    ("Separator", "Z"),
    ("Sharada", "Sharada"),
    ("Shavian", "Shavian"),
    ("Siddham", "Siddham"),
    ("Sinhala", "Sinhala"),
    ("Sk", "Sk"),
    ("Sm", "Sm"),
    ("So", "So"),
    ("Sogdian", "Sogdian"),
    ("Soyombo", "Soyombo"),
    ("Spaceseparator", "Zs"),
    ("Spacingmark", "Mc"),
    ("Sundanese", "Sundanese"),
    ("Surrogate", "Cs"),
    ("Symbol", "S"),
    ("Syriac", "Syriac"),
    ("Tagalog", "Tagalog"),
    ("Tagbanwa", "Tagbanwa"),
    ("Takri", "Takri"),
    ("Tamil", "Tamil"),
    ("Tangsa", "Tangsa"),
    ("Tangut", "Tangut"),
    ("Telugu", "Telugu"),
    ("Thaana", "Thaana"),
    ("Thai", "Thai"),
    ("Tibetan", "Tibetan"),
    ("Tifinagh", "Tifinagh"),
    ("Tirhuta", "Tirhuta"),
    ("Titlecaseletter", "Lt"),
    ("Toto", "Toto"),
    ("Ugaritic", "Ugaritic"),
    ("Unassigned", "Cn"),
    ("Uppercaseletter", "Lu"),
    ("Vai", "Vai"),
    ("Vithkuqi", "Vithkuqi"),
    ("Wancho", "Wancho"),
    ("Yezidi", "Yezidi"),
    ("Yi", "Yi"),
    ("Z", "Z"),
    ("Zl", "Zl"),
    ("Zp", "Zp"),
    ("Zs", "Zs"),
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

/// A class that matches nothing: what a class of surrogates (which no UTF-8
/// text holds) comes to.
const NEVER: &str = r"[^\x{0}-\x{10FFFF}]";

/// The flags `regexp.Compile` starts with (`syntax.Perl`) and `(?imsU)`
/// change: what they change is written into each atom, so the `regex` crate
/// needs none of its own.
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    /// `i`: case folding.
    fold: bool,
    /// `m`: `^` and `$` at line ends (Go's `OneLine` off).
    multi_line: bool,
    /// `s`: `.` takes `\n`.
    dot_nl: bool,
    /// `U`: repetitions lazy unless followed by `?`.
    non_greedy: bool,
}

/// What a translation reads of the Unicode tables, where Go's (Unicode
/// 15.0.0) and the `regex` crate's differ (see `go_unicode`), from least to
/// most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum Tables {
    /// Nothing.
    #[default]
    None,
    /// Case folding of characters that have no case in either (`(?i)1`,
    /// `(?i)[0-9]`): the same characters match in both, as long as the
    /// crate's tables are the ones `go_unicode` was computed for.
    UncasedFolding,
    /// A Unicode class, or case folding of a character that has a case.
    Read,
}

/// A pattern in the `regex` crate's syntax, and what it reads of the Unicode
/// tables.
#[derive(Debug)]
pub(crate) struct Translation {
    pub(crate) pattern: String,
    pub(crate) tables: Tables,
}

/// A node of the parse: what `repeatIsValid` walks, and the parser's two
/// pseudo-operators.
#[derive(Debug)]
enum Node {
    /// `(`, capturing or not, with the flags to restore at its `)`.
    LeftParen(Flags),
    /// `|`.
    VerticalBar,
    /// Anything else: a repetition carries its bounds, every node its
    /// sub-expressions, and its translation into the `regex` crate's syntax,
    /// which is `single` when a repetition operator applies to all of it.
    Expr {
        repeat: Option<(i32, i32)>,
        subs: Vec<Node>,
        out: String,
        single: bool,
    },
}

impl Node {
    fn atom(out: String) -> Node {
        Node::Expr {
            repeat: None,
            subs: Vec::new(),
            out,
            single: true,
        }
    }

    fn is_pseudo(&self) -> bool {
        !matches!(self, Node::Expr { .. })
    }

    fn out(&self) -> &str {
        match self {
            Node::Expr { out, .. } => out,
            _ => "",
        }
    }

    fn single(&self) -> bool {
        matches!(self, Node::Expr { single: true, .. })
    }
}

/// Go's `repeatIsValid`: the repetition and those inside it make at most `n`
/// copies of the innermost thing.
fn repeat_is_valid(node: &Node, mut n: i32) -> bool {
    let Node::Expr { repeat, subs, .. } = node else {
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

/// A rune for the `regex` crate: an ASCII letter or digit as it is, any other
/// as `\x{..}`.
fn rune(r: u32) -> String {
    match char::from_u32(r) {
        Some(c) if c.is_ascii_alphanumeric() => c.to_string(),
        _ => format!(r"\x{{{r:X}}}"),
    }
}

/// The class items for the runes `lo..=hi`, without the surrogates, which no
/// UTF-8 text holds and the `regex` crate cannot name.
fn range_items(lo: u32, hi: u32) -> String {
    let mut out = String::new();
    for (lo, hi) in [(lo, hi.min(0xD7FF)), (lo.max(0xE000), hi)] {
        if lo <= hi {
            out.push_str(&rune(lo));
            if lo < hi {
                out.push('-');
                out.push_str(&rune(hi));
            }
        }
    }
    out
}

/// A class of `items` (none matches nothing), case-folded under `i`.
fn class(items: &str, negated: bool, flags: Flags) -> String {
    let class = match (items.is_empty(), negated) {
        (true, false) => NEVER.to_owned(),
        (true, true) => r"[\x{0}-\x{10FFFF}]".to_owned(),
        (false, false) => format!("[{items}]"),
        (false, true) => format!("[^{items}]"),
    };
    folded(class, flags)
}

fn folded(atom: String, flags: Flags) -> String {
    if flags.fold { format!("(?i:{atom})") } else { atom }
}

/// Whether the class items of a Perl class hold a character that has a case:
/// all but `\d` and `\s` do.
fn perl_items_cased(items: &str) -> bool {
    !matches!(items, "0-9" | r"\x{9}\x{A}\x{C}\x{D}\x{20}")
}

/// Whether a POSIX class item (`[:alpha:]`, `[:^digit:]`) holds a character
/// that has a case: a negated one does, and those with ASCII letters.
fn posix_cased(item: &str) -> bool {
    let inner = &item[2..item.len() - 2];
    inner.starts_with('^')
        || matches!(
            inner,
            "alnum" | "alpha" | "ascii" | "graph" | "lower" | "print" | "upper" | "word" | "xdigit"
        )
}

/// The class items of a Perl class `\d`, `\s`, `\w` (ASCII, as in Go, whose
/// `\s` is `[\t\n\f\r ]`) or their negations.
fn perl_items(c: u8) -> &'static str {
    match c {
        b'd' => "0-9",
        b'D' => "[^0-9]",
        b's' => r"\x{9}\x{A}\x{C}\x{D}\x{20}",
        b'S' => r"[^\x{9}\x{A}\x{C}\x{D}\x{20}]",
        b'w' => "0-9A-Za-z_",
        _ => "[^0-9A-Za-z_]",
    }
}

/// The class items of a Unicode class by its `regex` crate name. Surrogates
/// (`Cs`), which the crate cannot name, match nothing in UTF-8 text.
fn unicode_items(name: &str, negated: bool) -> String {
    match (name, negated) {
        ("Cs", false) => String::new(),
        ("Cs", true) => r"\x{0}-\x{10FFFF}".to_owned(),
        (name, false) => format!(r"\p{{{name}}}"),
        (name, true) => format!(r"\P{{{name}}}"),
    }
}

#[derive(Default)]
struct Parser {
    stack: Vec<Node>,
    flags: Flags,
    tables: Tables,
}

impl Parser {
    fn push(&mut self, node: Node) {
        self.stack.push(node);
    }

    /// Notes what an atom reads of the Unicode tables.
    fn reads(&mut self, tables: Tables) {
        self.tables = self.tables.max(tables);
    }

    /// Notes an atom folded under `i`, whose characters have a case or not.
    fn folds(&mut self, cased: bool) {
        if self.flags.fold {
            self.reads(if cased {
                Tables::Read
            } else {
                Tables::UncasedFolding
            });
        }
    }

    /// A literal rune under the current flags.
    fn literal(&mut self, r: u32) {
        let atom = if (0xD800..=0xDFFF).contains(&r) {
            NEVER.to_owned()
        } else {
            self.folds(super::go_unicode::cased(r, r));
            folded(rune(r), self.flags)
        };
        self.push(Node::atom(atom));
    }

    /// The expressions above the topmost pseudo-operator, joined by `sep`.
    fn collapse(&mut self, sep: &str) {
        let i = self.stack.iter().rposition(Node::is_pseudo).map_or(0, |i| i + 1);
        let subs = self.stack.split_off(i);
        let single = subs.len() == 1 && subs[0].single();
        let out = subs.iter().map(Node::out).collect::<Vec<_>>().join(sep);
        self.push(Node::Expr {
            repeat: None,
            subs,
            out,
            single,
        });
    }

    /// Go's `concat`: the expressions above the topmost pseudo-operator
    /// become one.
    fn concat(&mut self) {
        self.collapse("");
    }

    /// Go's `alternate`, the same over `|`.
    fn alternate(&mut self) {
        self.collapse("|");
    }

    /// Go's `swapVerticalBar`: a `|` below the top moves to the top (Go merges
    /// two character classes around it instead, which matches the same).
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
        let Some(Node::LeftParen(flags)) = n.checked_sub(2).map(|i| &self.stack[i]) else {
            return Err(error(UNEXPECTED_PAREN, whole));
        };
        self.flags = *flags;
        let inner = self.stack.pop().expect("n >= 2");
        self.stack.pop();
        let out = format!("(?:{})", inner.out());
        self.push(Node::Expr {
            repeat: None,
            subs: vec![inner],
            out,
            single: true,
        });
        Ok(())
    }

    /// Go's `repeat`: `op` (`*`, `+`, `?` or `{..}`, with `bounds` for the
    /// last) applied to the top of the stack. `before` starts at the operator,
    /// `after` follows it; returns `after` past a non-greedy `?`.
    fn repeat<'a>(
        &mut self,
        op: &str,
        bounds: Option<(i32, i32)>,
        before: &str,
        mut after: &'a str,
        last_repeat: &str,
    ) -> Result<&'a str, Re2Error> {
        let mut lazy = self.flags.non_greedy;
        if let Some(rest) = after.strip_prefix('?') {
            after = rest;
            lazy = !lazy;
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
        let mut out = if sub.single() {
            sub.out().to_owned()
        } else {
            format!("(?:{})", sub.out())
        };
        out.push_str(op);
        if lazy {
            out.push('?');
        }
        let node = Node::Expr {
            repeat: bounds,
            subs: vec![sub],
            out,
            single: false,
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
#[cfg(test)]
pub(crate) fn check(pattern: &str) -> Result<(), Re2Error> {
    translate(pattern).map(|_| ())
}

/// `pattern` in the `regex` crate's syntax, matching what Go's `regexp`
/// matches with it (but for Unicode tables: Go has Unicode 15.0.0), or Go's
/// error. Every atom is written out with the flags it was parsed under: a
/// literal or class in `(?i:..)` under `i`, `^` / `$` as `\A` / `\z` or
/// `(?m:^)` / `(?m:$)`, `.` as `[^\n]` or `(?s:.)`, a repetition greedy or
/// lazy; Perl classes and word boundaries ASCII, as Go's are; every literal
/// rune of a class escaped (Go's `&&`, `--`, `~~` and `[` in a class are
/// literals); groups non-capturing (only whole matches count, and Go allows a
/// group name twice).
pub(crate) fn translate(pattern: &str) -> Result<Translation, Re2Error> {
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
                    p.push(Node::LeftParen(p.flags));
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
            b'^' => {
                let atom = if p.flags.multi_line { "(?m:^)" } else { r"\A" };
                p.push(Node::atom(atom.to_owned()));
                t = &t[1..];
            }
            b'$' => {
                let atom = if p.flags.multi_line { "(?m:$)" } else { r"\z" };
                p.push(Node::atom(atom.to_owned()));
                t = &t[1..];
            }
            b'.' => {
                let atom = if p.flags.dot_nl { "(?s:.)" } else { r"[^\n]" };
                p.push(Node::atom(atom.to_owned()));
                t = &t[1..];
            }
            b'[' => t = parse_class(&mut p, t)?,
            c @ (b'*' | b'+' | b'?') => {
                let before = t;
                let op = char::from(c).to_string();
                t = p.repeat(&op, None, before, &t[1..], last_repeat)?;
                repeat = before;
            }
            b'{' => {
                let before = t;
                match parse_repeat(t) {
                    None => {
                        p.literal(u32::from('{'));
                        t = &t[1..];
                    }
                    Some((min, max, after)) => {
                        if !(0..=1000).contains(&min) || max > 1000 || (max >= 0 && min > max) {
                            return Err(error(INVALID_REPEAT_SIZE, prefix(before, after)));
                        }
                        let op = match max {
                            -1 => format!("{{{min},}}"),
                            max if max == min => format!("{{{min}}}"),
                            max => format!("{{{min},{max}}}"),
                        };
                        t = p.repeat(&op, Some((min, max)), before, after, last_repeat)?;
                        repeat = before;
                    }
                }
            }
            b'\\' => t = parse_backslash(&mut p, t)?,
            _ => {
                let (c, rest) = next_rune(t);
                p.literal(u32::from(c));
                t = rest;
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
    Ok(Translation {
        pattern: p.stack[0].out().to_owned(),
        tables: p.tables,
    })
}

/// A `\` outside a class: `\A`, `\b`, `\B`, `\z`, `\Q..\E`, a Unicode or Perl
/// class, or a single-character escape.
fn parse_backslash<'a>(p: &mut Parser, t: &'a str) -> Result<&'a str, Re2Error> {
    let atom = match t.as_bytes().get(1) {
        Some(b'A') => Some(r"\A"),
        Some(b'b') => Some(r"(?-u:\b)"),
        Some(b'B') => Some(r"(?-u:\B)"),
        Some(b'z') => Some(r"\z"),
        _ => None,
    };
    if let Some(atom) = atom {
        p.push(Node::atom(atom.to_owned()));
        return Ok(&t[2..]);
    }
    match t.as_bytes().get(1) {
        // Any byte: not supported.
        Some(b'C') => return Err(error(INVALID_ESCAPE, &t[..2])),
        Some(b'Q') => {
            // `\Q ... \E`: the ... is always literals.
            let (lit, rest) = t[2..].split_once("\\E").unwrap_or((&t[2..], ""));
            for c in lit.chars() {
                p.literal(u32::from(c));
            }
            return Ok(rest);
        }
        _ => {}
    }
    if let Some((items, rest)) = parse_unicode_class(t)? {
        p.reads(Tables::Read);
        p.push(Node::atom(class(&items, false, p.flags)));
        return Ok(rest);
    }
    if let Some((items, rest)) = parse_perl_class_escape(t) {
        p.folds(perl_items_cased(items));
        p.push(Node::atom(class(items, false, p.flags)));
        return Ok(rest);
    }
    let (r, rest) = parse_escape(t)?;
    p.literal(r);
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
        p.push(Node::LeftParen(p.flags));
        return Ok(&s[end + 1..]);
    }

    let mut t = &s[2..];
    let mut flags = p.flags;
    // Under `-`, a flag turns off.
    let mut on = true;
    let mut saw_flag = false;
    while !t.is_empty() {
        let (c, rest) = next_rune(t);
        t = rest;
        match c {
            'i' => flags.fold = on,
            'm' => flags.multi_line = on,
            's' => flags.dot_nl = on,
            'U' => flags.non_greedy = on,
            '-' if on => {
                on = false;
                saw_flag = false;
                continue;
            }
            ':' | ')' if on || saw_flag => {
                if c == ':' {
                    p.push(Node::LeftParen(p.flags));
                }
                p.flags = flags;
                return Ok(t);
            }
            _ => break,
        }
        saw_flag = true;
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

/// Go's `parsePerlClassEscape`: `\d`, `\D`, `\s`, `\S`, `\w`, `\W`, as class
/// items.
fn parse_perl_class_escape(s: &str) -> Option<(&'static str, &str)> {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0] == b'\\' && matches!(b[1], b'd' | b'D' | b's' | b'S' | b'w' | b'W'))
        .then(|| (perl_items(b[1]), &s[2..]))
}

/// Go's `parseUnicodeClass`: a `\pN`, `\p{Name}` or `\P..` at the start of
/// `s`, as class items; `None` if `s` starts with none.
fn parse_unicode_class(s: &str) -> Result<Option<(String, &str)>, Re2Error> {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'\\' || (b[1] != b'p' && b[1] != b'P') {
        return Ok(None);
    }
    let mut negated = b[1] == b'P';
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
    let name = match name.strip_prefix('^') {
        Some(name) => {
            negated = !negated;
            name
        }
        None => name,
    };
    let canonical = canonical_name(name);
    let Ok(i) = UNICODE_CLASSES.binary_search_by(|(go, _)| go.cmp(&canonical.as_str())) else {
        return Err(error(INVALID_CHAR_CLASS_RANGE, seq));
    };
    Ok(Some((unicode_items(UNICODE_CLASSES[i].1, negated), rest)))
}

/// Go's `parseNamedClass`: a POSIX class `[:alnum:]` (or `[:^alnum:]`) at the
/// start of `s`, as a class item; `None` if `s` starts with no `[:..:]`.
fn parse_named_class(s: &str) -> Result<Option<(&str, &str)>, Re2Error> {
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
    // The `regex` crate has the same ASCII POSIX classes, in the same syntax.
    Ok(Some((name, rest)))
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
    let negated = t.starts_with('^');
    if negated {
        t = &t[1..];
    }
    let mut items = String::new();
    // Whether an item is a Unicode class, and whether one holds a character
    // that has a case.
    let (mut unicode, mut cased) = (false, false);
    // `]` (and `-`) may come first.
    let mut first = true;
    while t.is_empty() || !t.starts_with(']') || first {
        first = false;
        if t.len() > 2
            && let Some((item, rest)) = parse_named_class(t)?
        {
            cased |= posix_cased(item);
            items.push_str(item);
            t = rest;
            continue;
        }
        if let Some((item, rest)) = parse_unicode_class(t)? {
            unicode = true;
            items.push_str(&item);
            t = rest;
            continue;
        }
        if let Some((item, rest)) = parse_perl_class_escape(t) {
            cased |= perl_items_cased(item);
            items.push_str(item);
            t = rest;
            continue;
        }
        let range = t;
        let (lo, rest) = parse_class_char(t, s)?;
        t = rest;
        let mut hi = lo;
        // `[a-]` is `a` or `-`: a range only before something other than `]`.
        let b = t.as_bytes();
        if b.len() >= 2 && b[0] == b'-' && b[1] != b']' {
            let (h, rest) = parse_class_char(&t[1..], s)?;
            t = rest;
            if h < lo {
                return Err(error(INVALID_CHAR_CLASS_RANGE, prefix(range, t)));
            }
            hi = h;
        }
        cased |= super::go_unicode::cased(lo, hi);
        items.push_str(&range_items(lo, hi));
    }
    // A negated class folds its items, then negates: what it folds is theirs.
    if unicode {
        p.reads(Tables::Read);
    } else {
        p.folds(cased);
    }
    p.push(Node::atom(class(&items, negated, p.flags)));
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

    /// The translation spells out Go's meaning for the `regex` crate.
    #[test]
    fn translations_carry_go_s_meaning() {
        for (pattern, want) in [
            (r"\d\w\s", r"[0-9][0-9A-Za-z_][\x{9}\x{A}\x{C}\x{D}\x{20}]"),
            (r"\b\B", r"(?-u:\b)(?-u:\B)"),
            (r"[a&&[b]", r"[a\x{26}\x{26}\x{5B}b]"),
            (r"\<", r"\x{3C}"),
            (r"^.$", r"\A[^\n]\z"),
            (r"(?ms)^.$", r"(?m:^)(?s:.)(?m:$)"),
            (r"(?i)a(?-i)b", r"(?i:a)b"),
            (r"(?U)a*b*?", r"a*?b*"),
            (r"a(?i)*", r"a*"),
            (r"(?P<x>a)|b{2,}", r"(?:a)|b{2,}"),
            (r"\p{^Greek}\p{Letter}\p{Ascii}", r"[\P{Greek}][\p{L}][\p{ASCII}]"),
            (r"\x{D800}a{", r"[^\x{0}-\x{10FFFF}]a\x{7B}"),
        ] {
            let pattern_of = super::translate(pattern).map(|t| t.pattern);
            assert_eq!(pattern_of.as_deref(), Ok(want), "{pattern}");
        }
    }

    /// A Unicode class and the folding of a character that has a case read
    /// the Unicode tables; the folding of one that has none does not, as long
    /// as the `regex` crate's tables are the ones kube-cel knows.
    #[test]
    fn translations_tell_what_they_read_of_the_unicode_tables() {
        use super::Tables::{None, Read, UncasedFolding};
        for (pattern, want) in [
            ("a", None),
            ("(?i)", None),
            ("\\x{1C89}[à-ÿ]", None),
            (r"\pL", Read),
            (r"[a\p{Greek}]", Read),
            ("(?i)a", Read),
            (r"(?i)\x{1C89}", Read),
            (r"(?i)[\D]", Read),
            ("(?i)[[:alpha:]]", Read),
            ("(?i)[[:ascii:]]", Read),
            ("(?i)[[:^digit:]]", Read),
            ("(?i)[0-9A]", Read),
            ("(?i)1", UncasedFolding),
            (r"(?i)\d", UncasedFolding),
            ("(?i)[0-9]", UncasedFolding),
            ("(?i)[^0-9]", UncasedFolding),
            ("(?i)[[:digit:]]", UncasedFolding),
            ("(?i:1)a", UncasedFolding),
        ] {
            assert_eq!(super::translate(pattern).map(|t| t.tables), Ok(want), "{pattern}");
        }
    }

    #[test]
    fn unicode_classes_are_sorted_for_the_binary_search() {
        assert!(super::UNICODE_CLASSES.windows(2).all(|w| w[0] < w[1]));
    }
}
