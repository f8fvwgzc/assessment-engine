//! Text comparison: how different two versions of a title or description
//! are, and what exactly moved.
//!
//! Bag-of-words similarity alone is blind to the edits that matter most in
//! a work item: "must support SSO" → "must **not** support SSO" is one
//! token, "100 users" → "10 000 users" is one number, and deleting an
//! acceptance criterion is one line. So the magnitude combines, as
//! independent evidence (noisy-OR):
//!
//! * **lexical distance** — TF-IDF cosine (IDF over the request's whole
//!   neighbourhood, so shared boilerplate weighs little), word-shingle
//!   Jaccard (order), and for short texts a character edit distance whose
//!   weight fades out smoothly between one and two times
//!   [`TextPolicy::short_text_chars`];
//! * **semantic distance** — the embedding cosine, when the caller sends embeddings;
//! * **structure** — list items / acceptance criteria added, removed or
//!   reworded, and headings added or removed;
//! * **negation flips**, **modal shifts** (should → must) and **changed
//!   numbers**, counted only inside *reworded* passages: a sentence that is
//!   simply added ("… without an account", "OAuth 2.0") is an addition, not
//!   a flip or a changed number.
//!
//! A text is split into **passages** (sentences and list items). A passage's
//! identity is its term multiset *plus* which terms are negated and how
//! strongly they are required, so "must support SAML" → "must not support
//! SAML" is a reworded passage that propagation can match to the sub-issue
//! about SAML. When every passage, list item and heading is still there and
//! only the order or layout changed, the change is a reorder and its lexical
//! distance is capped near zero.
//!
//! Every pairwise step is bounded (see the `MAX_*` constants), so a long
//! description or a pasted blob costs time roughly linear in its length.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::policy::TextPolicy;
use crate::stats::{clamp01, noisy_or};

// ---- limits and constants ---------------------------------------------------

/// Tokens longer than this many characters are never typo-aligned: a
/// pasted key, hash or minified blob is not a misspelling, and the edit
/// distance of two long tokens is the expensive part of alignment.
const MAX_TYPO_TOKEN_CHARS: usize = 40;

/// Words of at most this many characters only count as typos of each other
/// for an adjacent swap or a slip to a neighbouring key: adding or dropping
/// a letter there is an affix (`http`/`https`, `sync`/`async`), and an
/// arbitrary substitution is another word (`mysql`/`mssql`).
const SHORT_WORD_CHARS: usize = 5;

/// Typo alignment is skipped when the words only in one text times the
/// words only in the other exceed this: that is a rewrite, not a typo edit.
const MAX_TYPO_PAIRS: usize = 250_000;

/// Edit-distance cells one typo-alignment pass may compute before it stops
/// aligning (a hard bound on its cost, whatever the input).
const MAX_TYPO_CELLS: usize = 4_000_000;

/// Two passages (or list items) are a reworded pair when their term sets
/// have at least this Jaccard similarity…
const PAIR_JACCARD: f64 = 0.5;

/// …or when the shorter has at most this many terms, they share a term, and
/// their texts have a character similarity of at least
/// [`SHORT_UNIT_CHAR_SIMILARITY`] ("Login with Google" → "Sign-in with
/// Google" is a reworded criterion, not a removed one).
const SHORT_UNIT_TERMS: usize = 3;

/// See [`SHORT_UNIT_TERMS`].
const SHORT_UNIT_CHAR_SIMILARITY: f64 = 0.6;

/// Passages longer than this many characters skip the short-passage
/// character similarity (they pair on term Jaccard only).
const MAX_PAIR_TEXT_CHARS: usize = 160;

/// Edit-distance cells one pairing may spend on short-passage similarity.
const MAX_PAIR_CHAR_CELLS: usize = 4_000_000;

/// Up to this many (removed × added) unit pairs, every pair is scored.
/// Above it, each removed unit is scored only against the added units near
/// its relative position ([`PAIR_WINDOW`]) and those sharing a rare term
/// ([`MAX_POSTINGS`]).
const MAX_FULL_PAIRING: usize = 512 * 512;

/// Half-width of the positional window for large pairings.
const PAIR_WINDOW: usize = 6;

/// A term in more than this many added units is too common to propose
/// candidates in a large pairing (it still counts in the similarity).
const MAX_POSTINGS: usize = 16;

/// Candidates kept per removed unit before the global greedy assignment
/// (small lists, then large ones).
const PAIR_TOP_K: usize = 16;

/// See [`PAIR_TOP_K`].
const PAIR_TOP_K_LARGE: usize = 3;

/// Hard cap on unit-pair similarity evaluations per pairing; units left
/// over once it is spent stay unpaired (removed / added).
const MAX_PAIR_EVALS: usize = 4_000_000;

/// The character edit distance is never computed for texts longer than this
/// (whatever [`TextPolicy::short_text_chars`] says): it is quadratic.
const MAX_CHAR_DISTANCE_CHARS: usize = 2048;

/// Lexical weights `(char, tfidf, shingle)` for short texts…
const SHORT_TEXT_WEIGHTS: (f64, f64, f64) = (0.4, 0.3, 0.3);

/// …and for long texts; in between they are interpolated linearly.
const LONG_TEXT_WEIGHTS: (f64, f64, f64) = (0.0, 0.6, 0.4);

/// The lexical distance of a pure reorder (same passages, list items and
/// headings, other order or layout) is capped at this.
const REORDER_LEXICAL_DISTANCE: f64 = 0.05;

/// English function words dropped from term statistics. Negations and modal
/// verbs are deliberately absent: they are read separately.
const STOPWORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "also", "am", "an", "and", "any",
    "are", "as", "at", "be", "because", "been", "before", "being", "below", "between", "both",
    "but", "by", "can", "did", "do", "does", "doing", "during", "each", "few", "for", "from",
    "further", "had", "has", "have", "having", "he", "her", "here", "his", "how", "i", "if", "in",
    "into", "is", "it", "its", "just", "me", "more", "most", "my", "of", "off", "on", "once",
    "only", "or", "other", "our", "out", "over", "own", "s", "same", "she", "so", "some", "such",
    "t", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "through", "to", "too", "under", "until", "up", "us", "very", "was", "we", "were", "what",
    "when", "where", "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

/// What moved in the structure of a Markdown text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StructureDiff {
    /// List items (checklist entries, bullets, numbered points) before and after.
    pub items_before: u32,
    pub items_after: u32,
    pub items_added: u32,
    pub items_removed: u32,
    /// Items matched to a reworded counterpart (term Jaccard ≥ 0.5, or a
    /// short item sharing a term with a close spelling).
    pub items_modified: u32,
    /// Checklist items ticked or unticked (progress, not scope).
    pub items_checked: u32,
    pub items_unchecked: u32,
    pub headings_added: u32,
    pub headings_removed: u32,
}

impl StructureDiff {
    /// Scope-relevant structural edits (ticking a box is not one).
    pub fn edits(&self) -> f64 {
        f64::from(self.items_added)
            + f64::from(self.items_removed)
            + 0.5 * f64::from(self.items_modified)
            + 0.5 * f64::from(self.headings_added + self.headings_removed)
    }
}

/// The comparison of two texts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDiff {
    /// How different the texts are, `[0, 1]` (0 = same meaning).
    pub magnitude: f64,
    /// The blended lexical similarity, `[0, 1]`.
    pub lexical_similarity: f64,
    /// `1 - edit distance / longer length`, when computed (texts shorter
    /// than twice [`TextPolicy::short_text_chars`]).
    pub char_similarity: Option<f64>,
    /// Jaccard similarity of the term sets.
    pub token_jaccard: f64,
    /// Jaccard similarity of word n-gram sets.
    pub shingle_similarity: f64,
    /// Cosine similarity of the TF-IDF vectors.
    pub tfidf_cosine: f64,
    /// Cosine similarity of the embeddings, when both were given.
    pub embedding_cosine: Option<f64>,
    pub structure: StructureDiff,
    /// Terms of reworded passages that became negated or stopped being
    /// negated (the term itself is on both sides).
    pub negation_flips: u32,
    /// Terms of reworded passages whose requirement strength changed (should ↔ must).
    pub modal_shifts: u32,
    /// Numbers (tokens with a digit: `30`, `2.0`, `200ms`, `oauth2`) that
    /// changed inside reworded passages.
    pub numbers_changed: u32,
    /// The most informative terms added and removed (stemmed).
    pub added_terms: Vec<String>,
    pub removed_terms: Vec<String>,
    pub chars_before: usize,
    pub chars_after: usize,
    /// Fewer than four terms on a side: lexical measures are noisy.
    pub low_information: bool,
    /// Passages (sentences, list items) only in the new text.
    pub passages_added: u32,
    /// Passages only in the old text.
    pub passages_removed: u32,
    /// Passages reworded (paired old/new), including negation and
    /// requirement-strength changes.
    pub passages_modified: u32,
    /// Same sentences, list items and headings, only their order or layout
    /// changed (the magnitude is then near zero).
    pub reordered: bool,
}

/// Document frequencies over a set of texts, for IDF weighting.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    docs: u32,
    df: BTreeMap<String, u32>,
}

impl Corpus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one document.
    pub fn add_document(&mut self, text: &str, policy: &TextPolicy) {
        let counts = term_counts(text, &Lexicon::new(policy));
        self.add_terms(counts.keys());
    }

    pub(crate) fn add_terms<'a>(&mut self, terms: impl IntoIterator<Item = &'a String>) {
        self.docs = self.docs.saturating_add(1);
        let unique: BTreeSet<&String> = terms.into_iter().collect();
        for t in unique {
            *self.df.entry(t.clone()).or_insert(0) += 1;
        }
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.docs as usize
    }

    pub fn is_empty(&self) -> bool {
        self.docs == 0
    }

    /// Smoothed inverse document frequency: `ln((1 + N) / (1 + df)) + 1`.
    pub fn idf(&self, term: &str) -> f64 {
        let df = f64::from(self.df.get(term).copied().unwrap_or(0));
        let n = f64::from(self.docs);
        ((1.0 + n) / (1.0 + df)).ln() + 1.0
    }
}

/// A sparse, L2-normalized term vector.
pub(crate) type SparseVec = BTreeMap<String, f64>;

/// Requirement strength of a modal verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    Weak,
    Strong,
}

/// A sentence or list item: the unit a description changes in.
#[derive(Debug, Clone)]
struct Passage {
    /// Identity: the term multiset, the negated terms and the requirement
    /// strengths (so a negated or strengthened sentence is a different passage).
    key: String,
    terms: BTreeMap<String, u32>,
    /// Terms negated within the passage ("not", "without", … up to three tokens before).
    negated: BTreeMap<String, u32>,
    /// Terms governed by a modal verb, with the strongest strength seen.
    modal: BTreeMap<String, Strength>,
    /// Number-like terms (with a digit).
    numbers: BTreeMap<String, u32>,
    /// The passage's tokens in order, space-separated.
    text: String,
}

impl Passage {
    /// The passage of a sentence's tokens and their roles; `None` when it
    /// has no term.
    fn build(tokens: &[String], roles: &[Role], text: String) -> Option<Self> {
        let mut terms = BTreeMap::new();
        let mut numbers = BTreeMap::new();
        let mut negated = BTreeMap::new();
        let mut modal: BTreeMap<String, Strength> = BTreeMap::new();
        let next_term = |i: usize| {
            roles[i + 1..roles.len().min(i + 4)]
                .iter()
                .find_map(|r| match r {
                    Role::Term(t) => Some(t.clone()),
                    _ => None,
                })
        };
        for (i, (tok, role)) in tokens.iter().zip(roles).enumerate() {
            match role {
                Role::Term(term) => {
                    *terms.entry(term.clone()).or_insert(0u32) += 1;
                    if has_digit(tok) {
                        *numbers.entry(term.clone()).or_insert(0u32) += 1;
                    }
                }
                Role::Negation => {
                    if let Some(t) = next_term(i) {
                        *negated.entry(t).or_insert(0u32) += 1;
                    }
                }
                Role::Modal(strength) => {
                    if let Some(t) = next_term(i) {
                        let e = modal.entry(t).or_insert(*strength);
                        if *strength > *e {
                            *e = *strength;
                        }
                    }
                }
                Role::Skip => {}
            }
        }
        if terms.is_empty() {
            return None;
        }
        let mut p = Self {
            key: String::new(),
            terms,
            negated,
            modal,
            numbers,
            text,
        };
        p.rekey();
        Some(p)
    }

    fn rekey(&mut self) {
        let mut key = String::with_capacity(self.text.len() + 16);
        fn count(key: &mut String, c: u32) {
            key.push('*');
            key.push_str(&c.to_string());
            key.push(' ');
        }
        for (t, c) in &self.terms {
            key.push_str(t);
            count(&mut key, *c);
        }
        for (t, c) in &self.negated {
            key.push('!');
            key.push_str(t);
            count(&mut key, *c);
        }
        for (t, s) in &self.modal {
            key.push(match s {
                Strength::Weak => '~',
                Strength::Strong => '^',
            });
            key.push_str(t);
            key.push(' ');
        }
        self.key = key;
    }
}

#[derive(Debug, Clone)]
struct ListItem {
    key: String,
    terms: BTreeSet<String>,
    checked: Option<bool>,
    /// The item's tokens in order, space-separated.
    text: String,
}

/// One text, analyzed once.
#[derive(Debug, Clone, Default)]
pub(crate) struct Analysis {
    raw: Vec<String>,
    pub(crate) counts: BTreeMap<String, u32>,
    items: Vec<ListItem>,
    headings: BTreeSet<String>,
    passages: Vec<Passage>,
    /// Every sentence's tokens (space-separated): the same multiset on both
    /// sides exactly when only the order or layout changed.
    segments: Vec<String>,
    normalized: String,
}

impl Analysis {
    pub(crate) fn term_count(&self) -> usize {
        self.counts.values().map(|c| *c as usize).sum()
    }

    pub(crate) fn char_count(&self) -> usize {
        self.normalized.chars().count()
    }
}

fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether a hyphen between `cur` and `rest` joins one identifier rather
/// than separating two words: letters then digits (`utf-8`, `sha-256`,
/// `covid-19`), or digits then one or two letters (`3-d`, `2-fa`). Such a
/// token is a name, not a quantity, so `3-D` and `3D` read the same and
/// neither is the number 3. `10-day` and `2026-11-07` still split.
fn hyphen_joins(cur: &str, rest: &[char]) -> bool {
    let (Some(prev), Some(&next)) = (cur.chars().last(), rest.first()) else {
        return false;
    };
    if prev.is_alphabetic() && next.is_ascii_digit() {
        return true;
    }
    if prev.is_ascii_digit() && next.is_alphabetic() {
        let letters = rest.iter().take_while(|c| c.is_alphabetic()).count();
        return letters <= 2 && rest.get(letters).is_none_or(|c| !is_token_char(*c));
    }
    false
}

/// Lowercased tokens: letters, digits and `_` of any script; `1.5` and
/// `10,000` stay one token; `3-D` and `UTF-8` become `3d` and `utf8`;
/// `n't` becomes `not` (`can't` → `can not`, `won't` → `will not`).
fn raw_tokens(text: &str) -> Vec<String> {
    let quote = |c: char| match c {
        '\u{2019}' | '\u{2018}' | '`' => '\'',
        c => c,
    };
    let mut chars: Vec<char> = Vec::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            chars.push(quote(c.to_ascii_lowercase()));
        } else {
            chars.extend(c.to_lowercase().map(quote));
        }
    }
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if is_token_char(c) {
            cur.push(c);
        } else if c == '\'' && cur.ends_with('n') && chars.get(i + 1) == Some(&'t') {
            if cur.ends_with("won") {
                cur.truncate(cur.len() - 3);
                cur.push_str("will");
            } else if !cur.ends_with("can") {
                cur.pop();
            }
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            // The `t` that follows completes "not".
            cur.push_str("no");
        } else if (c == '.' || c == ',')
            && cur.chars().last().is_some_and(|p| p.is_ascii_digit())
            && chars.get(i + 1).is_some_and(char::is_ascii_digit)
        {
            if c == '.' {
                cur.push(c);
            }
        } else if matches!(c, '-' | '\u{2010}' | '\u{2011}') && hyphen_joins(&cur, &chars[i + 1..])
        {
            // One identifier: keep accumulating.
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Words whose trailing `s` is not a plural, which [`stem`] keeps whole:
/// secure protocol variants (`https` is not `http`) and `news` (not `new`).
const STEM_KEEP: &[&str] = &["https", "ftps", "ldaps", "imaps", "smtps", "news"];

/// A light, deterministic English suffix stripper (ASCII words of four or
/// more letters only; other scripts pass through unchanged).
pub(crate) fn stem(word: &str) -> String {
    let n = word.len();
    if n < 4 || !word.bytes().all(|b| b.is_ascii_lowercase()) || STEM_KEEP.contains(&word) {
        return word.to_owned();
    }
    if n > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..n - 3]);
    }
    if n >= 6 && word.ends_with("ing") {
        return word[..n - 3].to_owned();
    }
    if n >= 5 && word.ends_with("ed") && !word.ends_with("eed") {
        return word[..n - 2].to_owned();
    }
    if n >= 5
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| word.ends_with(s))
    {
        return word[..n - 2].to_owned();
    }
    if word.ends_with('s') && !["ss", "us", "is"].iter().any(|s| word.ends_with(s)) {
        return word[..n - 1].to_owned();
    }
    if n >= 6 && word.ends_with("ly") {
        return word[..n - 2].to_owned();
    }
    word.to_owned()
}

fn is_number(token: &str) -> bool {
    token.starts_with(|c: char| c.is_ascii_digit()) && token.parse::<f64>().is_ok()
}

/// Number-like: a quantity, version or unit-suffixed value (`30`, `2.0`,
/// `200ms`, `oauth2`, `ipv6`). Never typo-aligned; counted as a number.
fn has_digit(token: &str) -> bool {
    token.chars().any(char::is_numeric)
}

/// Strips a leading list marker (`- `, `* `, `+ `, `1. `, `1) `) and an
/// optional checkbox from a line: `(checked, rest)` for list items.
fn parse_list_item(line: &str) -> Option<(Option<bool>, &str)> {
    let t = line.trim_start();
    let rest = if let Some(r) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        r
    } else {
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 4 {
            return None;
        }
        let after = &t[digits..];
        after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))?
    };
    let rest = rest.trim_start();
    if let Some(r) = rest.strip_prefix("[ ]") {
        Some((Some(false), r.trim()))
    } else if let Some(r) = rest
        .strip_prefix("[x]")
        .or_else(|| rest.strip_prefix("[X]"))
    {
        Some((Some(true), r.trim()))
    } else {
        Some((None, rest.trim()))
    }
}

/// Splits a line into sentences at `.`, `!`, `?` or `;` followed by
/// whitespace (or the end), unless a digit precedes it (`v2.` stays whole).
fn split_sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut it = line.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(c, '.' | '!' | '?' | ';') {
            let next_is_space = it.peek().is_none_or(|(_, n)| n.is_whitespace());
            let prev_is_digit = prev.is_some_and(|p| p.is_ascii_digit());
            if next_is_space && !prev_is_digit {
                let end = i + c.len_utf8();
                if !line[start..end].trim().is_empty() {
                    out.push(&line[start..end]);
                }
                start = end;
            }
        }
        prev = Some(c);
    }
    if !line[start..].trim().is_empty() {
        out.push(&line[start..]);
    }
    out
}

fn heading_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    rest.strip_prefix(' ').map(str::trim)
}

/// What a token contributes to a passage.
#[derive(Debug, Clone)]
enum Role {
    Term(String),
    Negation,
    Modal(Strength),
    Skip,
}

/// FNV-1a: a fast, deterministic hash for the small, fixed word lists of a
/// [`Lexicon`]. Maps keyed by user text keep the standard `RandomState`.
#[derive(Debug, Clone, Copy)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// A word the lexicon reads specially.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Word {
    Negation,
    Modal(Strength),
    Stop,
}

/// The words a text policy reads specially. Building one costs a few
/// microseconds, so a caller that analyses many texts under one policy
/// builds it once and passes it to [`analyze_with`] / [`term_counts`].
pub(crate) struct Lexicon<'p> {
    /// Negations, modal verbs and (when enabled) stop-words, in one lookup;
    /// a word in several lists reads as negation, then strong, then weak modal.
    words: HashMap<&'p str, Word, std::hash::BuildHasherDefault<Fnv>>,
}

impl<'p> Lexicon<'p> {
    pub(crate) fn new(policy: &'p TextPolicy) -> Self {
        let mut words: HashMap<&'p str, Word, std::hash::BuildHasherDefault<Fnv>> =
            HashMap::default();
        if policy.stopwords {
            words.extend(STOPWORDS.iter().map(|w| (*w, Word::Stop)));
        }
        let lists = [
            (&policy.weak_modals, Word::Modal(Strength::Weak)),
            (&policy.strong_modals, Word::Modal(Strength::Strong)),
            (&policy.negation_terms, Word::Negation),
        ];
        for (list, word) in lists {
            words.extend(list.iter().map(|w| (w.as_str(), word)));
        }
        Self { words }
    }

    /// What a raw token is: a term (stemmed; numbers kept whole), a
    /// negation, a modal verb, or nothing (stop-word, single letter).
    fn classify(&self, t: &str) -> Role {
        if is_number(t) {
            return Role::Term(t.to_owned());
        }
        match self.words.get(t) {
            Some(Word::Negation) => Role::Negation,
            Some(Word::Modal(s)) => Role::Modal(*s),
            Some(Word::Stop) => Role::Skip,
            None => {
                let mut chars = t.chars();
                if chars.next().is_none() || chars.next().is_none() {
                    Role::Skip
                } else {
                    Role::Term(stem(t))
                }
            }
        }
    }

    /// The term a raw token contributes, if any.
    fn term(&self, t: &str) -> Option<String> {
        match self.classify(t) {
            Role::Term(term) => Some(term),
            _ => None,
        }
    }
}

/// What a Markdown line is.
#[derive(Debug, Clone, Copy)]
enum LineKind {
    Heading,
    Item(Option<bool>),
    Plain,
}

/// [`analyze_with`] with a lexicon built for this one text. Tests only: the
/// engine builds one lexicon per call and passes it down.
#[cfg(test)]
pub(crate) fn analyze(text: &str, policy: &TextPolicy) -> Analysis {
    analyze_with(text, &Lexicon::new(policy))
}

/// The body of a Markdown line that carries terms, or `None` for a code
/// fence: a heading's text, a list item's text, or the line without its
/// indentation and quote markers.
fn line_body(line: &str) -> Option<(LineKind, &str)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return None;
    }
    Some(if let Some(h) = heading_text(line) {
        (LineKind::Heading, h)
    } else if let Some((checked, rest)) = parse_list_item(line) {
        (LineKind::Item(checked), rest)
    } else {
        (LineKind::Plain, trimmed.trim_start_matches('>'))
    })
}

/// Term counts from a list of terms: one sort instead of a map lookup per token.
fn count_terms(mut terms: Vec<String>) -> BTreeMap<String, u32> {
    terms.sort_unstable();
    let mut runs: Vec<(String, u32)> = Vec::new();
    for t in terms {
        match runs.last_mut() {
            Some((last, c)) if *last == t => *c += 1,
            _ => runs.push((t, 1)),
        }
    }
    runs.into_iter().collect()
}

/// Test-only: how many texts this thread has analysed ([`term_counts`] and
/// [`analyze_with`] calls), so a test can check what a cache saves.
#[cfg(test)]
pub(crate) mod analysed {
    use std::cell::Cell;

    thread_local! {
        static TEXTS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn count() {
        TEXTS.with(|c| c.set(c.get() + 1));
    }

    /// Texts analysed on this thread so far.
    pub(crate) fn texts() -> u64 {
        TEXTS.with(Cell::get)
    }
}

/// The term counts of a text, exactly as [`analyze_with`] computes
/// `Analysis::counts`, without building passages, segments, list items,
/// headings or the normalized copy. For texts whose only use is a TF-IDF
/// vector (context documents and titles).
pub(crate) fn term_counts(text: &str, lex: &Lexicon<'_>) -> BTreeMap<String, u32> {
    #[cfg(test)]
    analysed::count();
    let mut terms: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((_, body)) = line_body(line) else {
            continue;
        };
        for sentence in split_sentences(body) {
            for t in raw_tokens(sentence) {
                if let Role::Term(term) = lex.classify(&t) {
                    terms.push(term);
                }
            }
        }
    }
    count_terms(terms)
}

/// Analyzes a text once for every measure (each sentence is tokenized once
/// and each token's term computed once), with the caller's lexicon.
pub(crate) fn analyze_with(text: &str, lex: &Lexicon<'_>) -> Analysis {
    #[cfg(test)]
    analysed::count();
    let mut items = Vec::new();
    let mut headings = BTreeSet::new();
    let mut passages = Vec::new();
    let mut segments = Vec::new();
    let mut raw: Vec<String> = Vec::new();
    let mut all_terms: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((kind, body)) = line_body(line) else {
            continue;
        };
        let first_token = raw.len();
        let first_term = all_terms.len();
        for sentence in split_sentences(body) {
            let tokens = raw_tokens(sentence);
            if tokens.is_empty() {
                continue;
            }
            let roles: Vec<Role> = tokens.iter().map(|t| lex.classify(t)).collect();
            let joined = tokens.join(" ");
            if let Some(p) = Passage::build(&tokens, &roles, joined.clone()) {
                passages.push(p);
            }
            segments.push(joined);
            all_terms.extend(roles.into_iter().filter_map(|r| match r {
                Role::Term(t) => Some(t),
                _ => None,
            }));
            raw.extend(tokens);
        }
        let line_terms = &all_terms[first_term..];
        match kind {
            LineKind::Heading => {
                let key = line_terms.join(" ");
                if !key.is_empty() {
                    headings.insert(key);
                }
            }
            LineKind::Item(checked) => {
                let terms: BTreeSet<String> = line_terms.iter().cloned().collect();
                if !terms.is_empty() {
                    items.push(ListItem {
                        key: terms.iter().cloned().collect::<Vec<_>>().join(" "),
                        terms,
                        checked,
                        text: raw[first_token..].join(" "),
                    });
                }
            }
            LineKind::Plain => {}
        }
    }
    let counts = count_terms(all_terms);
    let mut normalized = String::with_capacity(text.len());
    for (k, w) in text.split_whitespace().enumerate() {
        if k > 0 {
            normalized.push(' ');
        }
        normalized.push_str(w);
    }
    let normalized = normalized.to_lowercase();
    Analysis {
        raw,
        counts,
        items,
        headings,
        passages,
        segments,
        normalized,
    }
}

/// TF-IDF vector with sublinear term frequency (`1 + ln tf`), L2-normalized.
pub(crate) fn tfidf(counts: &BTreeMap<String, u32>, corpus: &Corpus) -> SparseVec {
    let mut v: SparseVec = counts
        .iter()
        .filter(|(_, c)| **c > 0)
        .map(|(t, c)| (t.clone(), (1.0 + f64::from(*c).ln()) * corpus.idf(t)))
        .collect();
    let norm = v.values().map(|w| w * w).sum::<f64>().sqrt();
    if norm > 0.0 {
        for w in v.values_mut() {
            *w /= norm;
        }
    }
    v
}

/// Cosine of two normalized sparse vectors (non-negative weights), `[0, 1]`.
pub(crate) fn cosine(a: &SparseVec, b: &SparseVec) -> f64 {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let dot: f64 = small
        .iter()
        .filter_map(|(t, w)| large.get(t).map(|x| w * x))
        .sum();
    clamp01(dot)
}

/// Cosine of two dense embeddings; `None` if the dimensions differ, either
/// is empty or zero, or any component is not finite.
pub fn embedding_cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return None;
    }
    Some((dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0))
}

/// Levenshtein distance in characters (two-row dynamic programming).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Optimal-string-alignment distance (Levenshtein plus adjacent
/// transpositions: "teh" → "the" is one edit) when it is at most `max`,
/// else `None`.
///
/// Banded: only cells within `max` of the diagonal are computed, in three
/// rolling rows (`O(len)` memory), and it stops as soon as a whole row
/// exceeds `max`. Each computed cell is charged to `budget`; a pair whose
/// worst case does not fit in what is left empties the budget and is
/// reported as not within `max`.
fn osa_within(a: &[char], b: &[char], max: usize, budget: &mut usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max {
        return None;
    }
    if n == 0 || m == 0 {
        return Some(n.max(m));
    }
    let band = max.saturating_mul(2).saturating_add(1).min(m);
    if n.saturating_mul(band) > *budget {
        *budget = 0;
        return None;
    }
    let inf = max.saturating_add(1);
    let mut prev2 = vec![inf; m + 1];
    let mut prev = vec![inf; m + 1];
    let mut cur = vec![inf; m + 1];
    for (j, cell) in prev.iter_mut().enumerate().take(max.min(m) + 1) {
        *cell = j;
    }
    let mut cells = 0usize;
    for i in 1..=n {
        let lo = i.saturating_sub(max).max(1);
        let hi = i.saturating_add(max).min(m);
        cur[lo - 1] = if lo == 1 && i <= max { i } else { inf };
        let mut row_min = cur[lo - 1];
        for j in lo..=hi {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            let v = v.min(inf);
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if hi < m {
            cur[hi + 1] = inf;
        }
        cells += hi + 1 - lo;
        if row_min > max {
            *budget = budget.saturating_sub(cells);
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    *budget = budget.saturating_sub(cells);
    (prev[m] <= max).then_some(prev[m])
}

/// QWERTY neighbours of a lowercase ASCII letter.
fn keyboard_neighbours(c: char) -> &'static str {
    match c {
        'q' => "wa",
        'w' => "qeas",
        'e' => "wrsd",
        'r' => "etdf",
        't' => "ryfg",
        'y' => "tugh",
        'u' => "yihj",
        'i' => "uojk",
        'o' => "ipkl",
        'p' => "ol",
        'a' => "qwsz",
        's' => "weadzx",
        'd' => "erfsxc",
        'f' => "rtgdcv",
        'g' => "tyhfvb",
        'h' => "yujgbn",
        'j' => "uikhnm",
        'k' => "iojlm",
        'l' => "opk",
        'z' => "asx",
        'x' => "zsdc",
        'c' => "xdfv",
        'v' => "cfgb",
        'b' => "vghn",
        'n' => "bhjm",
        'm' => "njk",
        _ => "",
    }
}

/// Whether typing `b` for `a` is a plausible slip: neighbouring keys for
/// ASCII letters; any two non-ASCII letters (no layout is assumed for other
/// scripts).
fn key_slip(a: char, b: char) -> bool {
    if a.is_ascii() || b.is_ascii() {
        a.is_ascii_lowercase() && b.is_ascii_lowercase() && keyboard_neighbours(a).contains(b)
    } else {
        a.is_alphabetic() && b.is_alphabetic()
    }
}

/// Equal-length words that differ by one adjacent swap (`allow_slip`:
/// or by one neighbouring-key substitution).
fn swap_or_slip(w: &[char], c: &[char], allow_slip: bool) -> bool {
    if w.len() != c.len() {
        return false;
    }
    let mut diffs = w.iter().zip(c).enumerate().filter(|(_, (x, y))| x != y);
    match (diffs.next(), diffs.next(), diffs.next()) {
        (Some((_, (x, y))), None, None) => allow_slip && key_slip(*x, *y),
        (Some((p, (x1, y1))), Some((q, (x2, y2))), None) => q == p + 1 && x1 == y2 && x2 == y1,
        _ => false,
    }
}

/// The edit distance when `w` is a likely misspelling of `c`, else `None`:
///
/// * never for tokens with a digit (`200ms`/`500ms`, `oauth1`/`oauth2`) or
///   longer than [`MAX_TYPO_TOKEN_CHARS`];
/// * three letters: two adjacent letters swapped ("teh" → "the");
/// * up to [`SHORT_WORD_CHARS`] letters: same length, and one adjacent swap
///   or one slip to a neighbouring key (not `http`/`https`, `sync`/`async`,
///   `mysql`/`mssql`);
/// * longer words: OSA similarity ≥ 0.75 with at most one letter dropped or
///   doubled, and not a change of morpheme ([`changes_morpheme`]:
///   `review`/`preview`, `increase`/`decrease`, `maximize`/`minimize`).
fn is_typo_of(w: &[char], c: &[char], budget: &mut usize) -> Option<usize> {
    let short = w.len().min(c.len());
    let long = w.len().max(c.len());
    if !(3..=MAX_TYPO_TOKEN_CHARS).contains(&long) || long - short > 1 {
        return None;
    }
    if w.iter().chain(c).any(|ch| ch.is_numeric()) {
        return None;
    }
    if long == 3 {
        return swap_or_slip(w, c, false).then_some(1);
    }
    if short <= SHORT_WORD_CHARS {
        return swap_or_slip(w, c, true).then_some(1);
    }
    if changes_morpheme(w, c) {
        return None;
    }
    osa_within(w, c, long / 4, budget)
}

/// Whether two long words differ the way two *different* words do rather
/// than the way a slip of the hand does:
///
/// * another first letter (`increase`/`decrease`, `internal`/`external`,
///   `review`/`preview`), unless the first two letters are swapped;
/// * one run of two or more letters replaced by other letters
///   (`maximize`/`minimize`, `interstate`/`intrastate`,
///   `hyperthermia`/`hypothermia`), unless it is one adjacent swap;
/// * the prefixes `micro`/`macro`.
///
/// A typo changes one letter, swaps two, or drops or doubles one.
fn changes_morpheme(w: &[char], c: &[char]) -> bool {
    let swapped =
        |a: &[char], b: &[char]| a.len() == 2 && b.len() == 2 && a[0] == b[1] && a[1] == b[0];
    if w[0] != c[0] {
        return !(w.len() == c.len() && swapped(&w[..2], &c[..2]) && w[2..] == c[2..]);
    }
    let prefix = w.iter().zip(c).take_while(|(a, b)| a == b).count();
    let room = w.len().min(c.len()) - prefix;
    let suffix = w
        .iter()
        .rev()
        .zip(c.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&w[prefix..w.len() - suffix], &c[prefix..c.len() - suffix]);
    if a.len().min(b.len()) >= 1 && a.len().max(b.len()) >= 2 {
        return !swapped(a, b) && a.iter().zip(b).all(|(x, y)| x != y);
    }
    let pair = |x: &[char], y: &[char]| {
        x.iter().copied().take(5).eq("micro".chars())
            && y.iter().copied().take(5).eq("macro".chars())
    };
    pair(w, c) || pair(c, w)
}

/// For each word only in `ours` that is a likely misspelling of a word only
/// in `theirs`, the word it should be read as (the closest, then the first).
/// Number-like and overlong tokens are never aligned. Skipped for large
/// rewrites, which are not typo edits, and stops once it has spent
/// [`MAX_TYPO_CELLS`].
fn typo_map<'a>(
    ours: impl Iterator<Item = &'a String>,
    theirs: impl Iterator<Item = &'a String>,
) -> BTreeMap<String, String> {
    let alignable = |t: &String| {
        !has_digit(t)
            && t.len() <= 4 * MAX_TYPO_TOKEN_CHARS
            && t.chars().count() <= MAX_TYPO_TOKEN_CHARS
    };
    let ours: HashSet<&String> = ours.collect();
    let theirs: HashSet<&String> = theirs.collect();
    let only = |x: &HashSet<&'a String>, y: &HashSet<&'a String>| -> Vec<&'a String> {
        x.iter()
            .copied()
            .filter(|t| !y.contains(t) && alignable(t))
            .collect()
    };
    let mut only_ours = only(&ours, &theirs);
    let mut only_theirs = only(&theirs, &ours);
    let mut map = BTreeMap::new();
    if only_ours.is_empty()
        || only_theirs.is_empty()
        || only_ours.len().saturating_mul(only_theirs.len()) > MAX_TYPO_PAIRS
    {
        return map;
    }
    // Sorted, so the result (and where a spent budget stops) is deterministic.
    only_ours.sort_unstable();
    only_theirs.sort_unstable();
    let mut by_len: BTreeMap<usize, Vec<(&String, Vec<char>)>> = BTreeMap::new();
    for t in only_theirs {
        let c: Vec<char> = t.chars().collect();
        by_len.entry(c.len()).or_default().push((t, c));
    }
    let mut budget = MAX_TYPO_CELLS;
    for word in only_ours {
        if budget == 0 {
            break;
        }
        let w: Vec<char> = word.chars().collect();
        let mut best: Option<(usize, &String)> = None;
        for (_, bucket) in by_len.range(w.len().saturating_sub(1)..=w.len() + 1) {
            for (cand, c) in bucket {
                let Some(d) = is_typo_of(&w, c, &mut budget) else {
                    continue;
                };
                if best.is_none_or(|(bd, bc)| d < bd || (d == bd && *cand < bc)) {
                    best = Some((d, cand));
                }
            }
        }
        if let Some((_, cand)) = best {
            map.insert(word.clone(), cand.clone());
        }
    }
    map
}

/// Rewrites `b` so that words that are misspellings of words in `a` read as
/// those words: a typo fix then changes neither the term statistics nor the
/// added/removed terms, only the character-level distance.
pub(crate) fn align_typos(a: &Analysis, b: &mut Analysis, lex: &Lexicon<'_>) {
    let raws = typo_map(b.raw.iter(), a.raw.iter());
    let mut terms: BTreeMap<String, String> = BTreeMap::new();
    for tok in &mut b.raw {
        let Some(fixed) = raws.get(tok) else { continue };
        let from = lex.term(tok);
        let to = lex.term(fixed);
        if let Some(f) = &from
            && let Some(c) = b.counts.get_mut(f)
        {
            *c = c.saturating_sub(1);
        }
        if let Some(t) = &to {
            *b.counts.entry(t.clone()).or_insert(0) += 1;
        }
        if let (Some(f), Some(t)) = (from, to) {
            terms.insert(f, t);
        }
        *tok = fixed.clone();
    }
    b.counts.retain(|_, c| *c > 0);
    for (k, v) in typo_map(b.counts.keys(), a.counts.keys()) {
        terms.entry(k).or_insert(v);
    }
    if terms.is_empty() {
        return;
    }
    let rename = |t: &String| terms.get(t).cloned().unwrap_or_else(|| t.clone());
    let remap = |m: &BTreeMap<String, u32>| -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for (k, v) in m {
            *out.entry(rename(k)).or_insert(0) += v;
        }
        out
    };
    b.counts = remap(&b.counts);
    for passage in &mut b.passages {
        passage.terms = remap(&passage.terms);
        passage.negated = remap(&passage.negated);
        let mut modal: BTreeMap<String, Strength> = BTreeMap::new();
        for (k, s) in &passage.modal {
            let e = modal.entry(rename(k)).or_insert(*s);
            if *s > *e {
                *e = *s;
            }
        }
        passage.modal = modal;
        passage.rekey();
    }
    for item in &mut b.items {
        item.terms = item.terms.iter().map(rename).collect();
        item.key = item.terms.iter().cloned().collect::<Vec<_>>().join(" ");
    }
    b.headings = b
        .headings
        .iter()
        .map(|h| {
            h.split(' ')
                .map(|t| terms.get(t).map_or(t, String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
}

fn jaccard<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    if union == 0.0 { 1.0 } else { inter / union }
}

/// Jaccard similarity of two maps' key sets (a sorted merge; 1 when both are empty).
fn key_jaccard<V>(a: &BTreeMap<String, V>, b: &BTreeMap<String, V>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut x, mut y) = (a.keys().peekable(), b.keys().peekable());
    let mut inter = 0usize;
    while let (Some(p), Some(q)) = (x.peek(), y.peek()) {
        match p.cmp(q) {
            Ordering::Less => {
                x.next();
            }
            Ordering::Greater => {
                y.next();
            }
            Ordering::Equal => {
                inter += 1;
                x.next();
                y.next();
            }
        }
    }
    inter as f64 / (a.len() + b.len() - inter) as f64
}

fn shingles(raw: &[String], k: usize) -> BTreeSet<String> {
    if raw.len() < k || k <= 1 {
        return raw.iter().cloned().collect();
    }
    raw.windows(k).map(|w| w.join(" ")).collect()
}

/// Multiset difference sizes `(only in a, only in b)`.
fn multiset_diff(a: &BTreeMap<String, u32>, b: &BTreeMap<String, u32>) -> (u32, u32) {
    let mut only_a = 0u32;
    let mut only_b = 0u32;
    for (k, ca) in a {
        let cb = b.get(k).copied().unwrap_or(0);
        only_a = only_a.saturating_add(ca.saturating_sub(cb));
    }
    for (k, cb) in b {
        let ca = a.get(k).copied().unwrap_or(0);
        only_b = only_b.saturating_add(cb.saturating_sub(ca));
    }
    (only_a, only_b)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

// ---- pairing ----------------------------------------------------------------

/// Matches equal keys, earliest first: each `a` key's partner in `b`, and
/// which `b` entries were taken. `O(n)` with a hash index.
fn match_exact(a: &[&str], b: &[&str]) -> (Vec<Option<usize>>, Vec<bool>) {
    let mut slots: HashMap<&str, VecDeque<usize>> = HashMap::with_capacity(b.len());
    for (j, k) in b.iter().enumerate() {
        slots.entry(k).or_default().push_back(j);
    }
    let mut used = vec![false; b.len()];
    let partner = a
        .iter()
        .map(|k| {
            let j = slots.get_mut(k)?.pop_front()?;
            used[j] = true;
            Some(j)
        })
        .collect();
    (partner, used)
}

/// A passage or list item as the pairing sees it.
struct Unit<'a> {
    /// Its distinct terms, sorted.
    terms: Vec<&'a str>,
    /// Its tokens in order.
    text: &'a str,
}

impl Unit<'_> {
    /// The characters of a short-enough text, for the short-unit rule.
    fn short_chars(&self) -> Option<Vec<char>> {
        if self.text.len() > 4 * MAX_PAIR_TEXT_CHARS {
            return None;
        }
        let c: Vec<char> = self.text.chars().collect();
        (c.len() <= MAX_PAIR_TEXT_CHARS).then_some(c)
    }
}

fn sorted_intersection(x: &[u32], y: &[u32]) -> usize {
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
        }
    }
    n
}

/// A proposed reworded pair.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// Term Jaccard.
    jaccard: f64,
    /// Character similarity, when the short-unit rule was needed (else 0).
    chars: f64,
    /// Distance from the removed unit's expected relative position.
    offset: usize,
    i: usize,
    j: usize,
}

fn candidate_order(p: &Candidate, q: &Candidate) -> Ordering {
    q.jaccard
        .total_cmp(&p.jaccard)
        .then(q.chars.total_cmp(&p.chars))
        .then(p.offset.cmp(&q.offset))
        .then(p.i.cmp(&q.i))
        .then(p.j.cmp(&q.j))
}

/// `(jaccard, char similarity)` when `x` and `y` are a plausible reworded
/// pair: term Jaccard ≥ [`PAIR_JACCARD`], or a short unit sharing a term
/// with a close spelling ([`SHORT_UNIT_TERMS`]).
fn pair_score<'c>(
    x: &[u32],
    y: &[u32],
    chars: impl FnOnce() -> (Option<&'c [char]>, Option<&'c [char]>),
    char_budget: &mut usize,
) -> Option<(f64, f64)> {
    let inter = sorted_intersection(x, y);
    if inter == 0 {
        return None;
    }
    let union = x.len() + y.len() - inter;
    let jaccard = inter as f64 / union as f64;
    if jaccard >= PAIR_JACCARD {
        return Some((jaccard, 0.0));
    }
    if x.len().min(y.len()) > SHORT_UNIT_TERMS {
        return None;
    }
    let (xc, yc) = chars();
    let (xc, yc) = (xc?, yc?);
    let longest = xc.len().max(yc.len());
    if longest == 0 {
        return None;
    }
    // similarity ≥ s  ⇔  distance ≤ (1 − s) · longest
    let max = ((1.0 - SHORT_UNIT_CHAR_SIMILARITY) * longest as f64 + 1e-9).floor() as usize;
    let d = osa_within(xc, yc, max, char_budget)?;
    Some((jaccard, 1.0 - d as f64 / longest as f64))
}

/// Pairs reworded units: `(index in gone, index in new)`, sorted by `gone`.
///
/// Every candidate is scored ([`pair_score`]); each removed unit keeps its
/// best few ([`PAIR_TOP_K`], large lists [`PAIR_TOP_K_LARGE`]), and pairs
/// are then taken greedily, best first (ties: closer relative position,
/// then earlier). Small lists score every pair; large ones score a
/// positional window plus the units sharing a rare term, under a hard
/// evaluation cap ([`MAX_PAIR_EVALS`]), so the cost stays near-linear.
fn pair_reworded<'a>(gone: &[Unit<'a>], new: &[Unit<'a>]) -> Vec<(usize, usize)> {
    let (n, m) = (gone.len(), new.len());
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let full = n.saturating_mul(m) <= MAX_FULL_PAIRING;
    let top_k = if full { PAIR_TOP_K } else { PAIR_TOP_K_LARGE };
    let window = if full {
        m
    } else {
        (MAX_PAIR_EVALS / n.saturating_mul(4).max(1)).min(PAIR_WINDOW)
    };
    // Terms as small integers: sorted-id intersections are cheap.
    let mut ids: HashMap<&'a str, u32> = HashMap::new();
    let mut intern = |u: &Unit<'a>| -> Vec<u32> {
        let mut v: Vec<u32> = u
            .terms
            .iter()
            .map(|t| {
                let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                *ids.entry(*t).or_insert(next)
            })
            .collect();
        v.sort_unstable();
        v
    };
    let new_terms: Vec<Vec<u32>> = new.iter().map(&mut intern).collect();
    let gone_terms: Vec<Vec<u32>> = gone.iter().map(&mut intern).collect();
    let mut postings: Vec<Vec<usize>> = Vec::new();
    if !full {
        postings.resize_with(ids.len(), Vec::new);
        for (j, terms) in new_terms.iter().enumerate() {
            for &t in terms {
                postings[t as usize].push(j);
            }
        }
    }
    // Characters are only needed by the short-unit rule: computed on demand.
    let new_chars: Vec<OnceCell<Option<Vec<char>>>> = (0..m).map(|_| OnceCell::new()).collect();
    let mut char_budget = MAX_PAIR_CHAR_CELLS;
    let mut evals = 0usize;
    let mut seen = vec![usize::MAX; m];
    let mut js: Vec<usize> = Vec::new();
    let mut mine: Vec<Candidate> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (i, x) in gone.iter().enumerate() {
        if evals >= MAX_PAIR_EVALS {
            break;
        }
        let center = i.checked_mul(m).map_or(m - 1, |x| x / n);
        js.clear();
        if full {
            js.extend(0..m);
        } else {
            js.extend(center.saturating_sub(window)..center.saturating_add(window + 1).min(m));
            for &t in &gone_terms[i] {
                if let Some(p) = postings.get(t as usize)
                    && p.len() <= MAX_POSTINGS
                {
                    js.extend_from_slice(p);
                }
            }
        }
        let xc: OnceCell<Option<Vec<char>>> = OnceCell::new();
        mine.clear();
        for &j in &js {
            if seen[j] == i {
                continue;
            }
            seen[j] = i;
            evals += 1;
            let chars = || {
                let xc = xc.get_or_init(|| x.short_chars());
                let yc = new_chars[j].get_or_init(|| new[j].short_chars());
                (xc.as_deref(), yc.as_deref())
            };
            if let Some((jaccard, chars)) =
                pair_score(&gone_terms[i], &new_terms[j], chars, &mut char_budget)
            {
                mine.push(Candidate {
                    jaccard,
                    chars,
                    offset: j.abs_diff(center),
                    i,
                    j,
                });
            }
        }
        if mine.len() > top_k {
            mine.select_nth_unstable_by(top_k - 1, candidate_order);
            mine.truncate(top_k);
        }
        candidates.append(&mut mine);
    }
    candidates.sort_by(candidate_order);
    let mut paired_gone = vec![false; n];
    let mut paired_new = vec![false; m];
    let mut pairs = Vec::new();
    for c in candidates {
        if !paired_gone[c.i] && !paired_new[c.j] {
            paired_gone[c.i] = true;
            paired_new[c.j] = true;
            pairs.push((c.i, c.j));
        }
    }
    pairs.sort_unstable();
    pairs
}

/// How the passages of two texts correspond.
#[derive(Debug, Default)]
struct PassagePairing {
    /// Reworded `(a, b)` passage pairs (identity differs, content close).
    reworded: Vec<(usize, usize)>,
    /// Passages of `a` with no counterpart.
    removed: Vec<usize>,
    /// Passages of `b` with no counterpart.
    added: Vec<usize>,
}

fn passage_keys(x: &Analysis) -> Vec<&str> {
    x.passages.iter().map(|p| p.key.as_str()).collect()
}

fn passage_units<'a>(x: &'a Analysis, idx: &[usize]) -> Vec<Unit<'a>> {
    idx.iter()
        .map(|&k| Unit {
            terms: x.passages[k].terms.keys().map(String::as_str).collect(),
            text: &x.passages[k].text,
        })
        .collect()
}

fn pair_passages(a: &Analysis, b: &Analysis) -> PassagePairing {
    let (partner, used) = match_exact(&passage_keys(a), &passage_keys(b));
    let gone: Vec<usize> = (0..a.passages.len())
        .filter(|i| partner[*i].is_none())
        .collect();
    let new: Vec<usize> = (0..b.passages.len()).filter(|j| !used[*j]).collect();
    let local = pair_reworded(&passage_units(a, &gone), &passage_units(b, &new));
    let mut paired_gone = vec![false; gone.len()];
    let mut paired_new = vec![false; new.len()];
    let mut reworded = Vec::with_capacity(local.len());
    for (i, j) in local {
        paired_gone[i] = true;
        paired_new[j] = true;
        reworded.push((gone[i], new[j]));
    }
    let unpaired = |idx: &[usize], paired: &[bool]| -> Vec<usize> {
        idx.iter()
            .zip(paired)
            .filter(|(_, p)| !**p)
            .map(|(k, _)| *k)
            .collect()
    };
    PassagePairing {
        removed: unpaired(&gone, &paired_gone),
        added: unpaired(&new, &paired_new),
        reworded,
    }
}

fn item_keys(x: &Analysis) -> Vec<&str> {
    x.items.iter().map(|i| i.key.as_str()).collect()
}

fn item_units<'a>(items: &[&'a ListItem]) -> Vec<Unit<'a>> {
    items
        .iter()
        .map(|it| Unit {
            terms: it.terms.iter().map(String::as_str).collect(),
            text: &it.text,
        })
        .collect()
}

/// List items with their check state, sorted.
fn item_states(x: &Analysis) -> Vec<(&str, Option<bool>)> {
    let mut v: Vec<(&str, Option<bool>)> = x
        .items
        .iter()
        .map(|i| (i.key.as_str(), i.checked))
        .collect();
    v.sort_unstable();
    v
}

fn diff_structure(a: &Analysis, b: &Analysis) -> StructureDiff {
    let mut diff = StructureDiff {
        items_before: to_u32(a.items.len()),
        items_after: to_u32(b.items.len()),
        ..StructureDiff::default()
    };
    let (partner, used) = match_exact(&item_keys(a), &item_keys(b));
    let mut gone = Vec::new();
    for (item, p) in a.items.iter().zip(&partner) {
        match p {
            Some(j) => match (item.checked, b.items[*j].checked) {
                (Some(false) | None, Some(true)) => diff.items_checked += 1,
                (Some(true), Some(false) | None) => diff.items_unchecked += 1,
                _ => {}
            },
            None => gone.push(item),
        }
    }
    let new: Vec<&ListItem> = b
        .items
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(it, _)| it)
        .collect();
    let pairs = pair_reworded(&item_units(&gone), &item_units(&new));
    diff.items_modified = to_u32(pairs.len());
    diff.items_removed = to_u32(gone.len() - pairs.len());
    diff.items_added = to_u32(new.len() - pairs.len());
    diff.headings_added = to_u32(b.headings.difference(&a.headings).count());
    diff.headings_removed = to_u32(a.headings.difference(&b.headings).count());
    diff
}

/// The terms `b` has more of than `a` (as counts).
fn count_delta(a: &BTreeMap<String, u32>, b: &BTreeMap<String, u32>) -> BTreeMap<String, u32> {
    b.iter()
        .filter_map(|(t, cb)| {
            let extra = cb.saturating_sub(a.get(t).copied().unwrap_or(0));
            (extra > 0).then(|| (t.clone(), extra))
        })
        .collect()
}

// ---- deltas for propagation ---------------------------------------------------

/// Term → `(passage number, weight)` over every changed passage's TF-IDF
/// vector, numbered removed first, then added, then modified.
#[derive(Debug, Clone, Default)]
struct PassageIndex {
    postings: HashMap<String, Vec<(usize, f64)>>,
    removed: usize,
    added: usize,
}

impl PassageIndex {
    fn build(removed: &[SparseVec], added: &[SparseVec], modified: &[SparseVec]) -> Self {
        let mut postings: HashMap<String, Vec<(usize, f64)>> = HashMap::new();
        for (p, v) in removed.iter().chain(added).chain(modified).enumerate() {
            for (t, w) in v {
                postings.entry(t.clone()).or_default().push((p, *w));
            }
        }
        Self {
            postings,
            removed: removed.len(),
            added: added.len(),
        }
    }
}

/// What a text change added, removed and reworded (passages as TF-IDF
/// vectors, held in an inverted index), for measuring how relevant the
/// change is to another entity's text.
#[derive(Debug, Clone, Default)]
pub(crate) struct TextDelta {
    /// `after − before` of the entity's embedding, L2-normalized, when both exist.
    pub(crate) embedding_delta: Option<Vec<f64>>,
    /// Removed passages (only in the old text), added passages (only in the
    /// new one) and modified passages (reworded, negated, strengthened or
    /// weakened; old and new terms together). [`TextDelta::overlap`] costs
    /// the postings of the target's terms, not every changed passage.
    index: PassageIndex,
    /// The same passages as vectors, for callers that need them one by one
    /// (passage ownership among siblings, title matching).
    pub(crate) removed: Vec<SparseVec>,
    pub(crate) added: Vec<SparseVec>,
    pub(crate) modified: Vec<SparseVec>,
}

/// How much a text change touches another text: the best cosine between
/// that text and any one changed passage (max-passage scoring, so a child
/// about one changed sentence is not diluted by the rest of a rewrite).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Overlap {
    pub removed: f64,
    pub added: f64,
    pub modified: f64,
}

impl Overlap {
    pub(crate) fn max(&self) -> f64 {
        self.removed.max(self.added).max(self.modified)
    }
}

impl TextDelta {
    fn new(
        added: Vec<SparseVec>,
        removed: Vec<SparseVec>,
        modified: Vec<SparseVec>,
        embedding_delta: Option<Vec<f64>>,
    ) -> Self {
        Self {
            embedding_delta,
            index: PassageIndex::build(&removed, &added, &modified),
            removed,
            added,
            modified,
        }
    }

    /// `(added, removed, modified)` passage counts (every indexed vector is non-empty).
    #[cfg(test)]
    fn counts(&self) -> (usize, usize, usize) {
        let ids: BTreeSet<usize> = self
            .index
            .postings
            .values()
            .flatten()
            .map(|(p, _)| *p)
            .collect();
        let modified = ids
            .iter()
            .filter(|p| **p >= self.index.removed + self.index.added)
            .count();
        (self.index.added, self.index.removed, modified)
    }

    pub(crate) fn overlap(&self, doc: &SparseVec) -> Overlap {
        let mut dots: HashMap<usize, f64> = HashMap::new();
        for (t, w) in doc {
            if let Some(ps) = self.index.postings.get(t.as_str()) {
                for &(p, pw) in ps {
                    *dots.entry(p).or_insert(0.0) += w * pw;
                }
            }
        }
        let mut o = Overlap::default();
        let (r, a) = (self.index.removed, self.index.added);
        for (p, dot) in dots {
            let v = clamp01(dot);
            let slot = if p < r {
                &mut o.removed
            } else if p < r + a {
                &mut o.added
            } else {
                &mut o.modified
            };
            *slot = slot.max(v);
        }
        o
    }

    /// A delta that only adds one passage (a comment, a new entity's text).
    pub(crate) fn addition(terms: &BTreeMap<String, u32>, corpus: &Corpus) -> Self {
        let v = tfidf(terms, corpus);
        let added = if v.is_empty() { Vec::new() } else { vec![v] };
        Self::new(added, Vec::new(), Vec::new(), None)
    }
}

pub(crate) fn text_delta(
    a: &Analysis,
    b: &Analysis,
    corpus: &Corpus,
    embeddings: Option<(&[f32], &[f32])>,
) -> TextDelta {
    let pairing = pair_passages(a, b);
    let modified = pairing
        .reworded
        .iter()
        .map(|&(i, j)| {
            let mut union = a.passages[i].terms.clone();
            for (t, c) in &b.passages[j].terms {
                let e = union.entry(t.clone()).or_insert(0);
                *e = (*e).max(*c);
            }
            tfidf(&union, corpus)
        })
        .collect();
    let vectors = |x: &Analysis, idx: &[usize]| -> Vec<SparseVec> {
        idx.iter()
            .map(|&k| tfidf(&x.passages[k].terms, corpus))
            .filter(|v| !v.is_empty())
            .collect()
    };
    TextDelta::new(
        vectors(b, &pairing.added),
        vectors(a, &pairing.removed),
        modified,
        embeddings.and_then(|(x, y)| embedding_direction(x, y)),
    )
}

/// `after − before`, L2-normalized; `None` when unusable or (near) zero.
fn embedding_direction(before: &[f32], after: &[f32]) -> Option<Vec<f64>> {
    if before.is_empty() || before.len() != after.len() {
        return None;
    }
    let mut d = Vec::with_capacity(before.len());
    for (x, y) in before.iter().zip(after) {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        d.push(f64::from(*y) - f64::from(*x));
    }
    let norm = d.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm < 1e-9 {
        return None;
    }
    for v in &mut d {
        *v /= norm;
    }
    Some(d)
}

/// `|cos|` between a change direction and an entity's embedding: how much
/// the change moves along what the entity is about.
pub(crate) fn direction_alignment(direction: &[f64], embedding: &[f32]) -> Option<f64> {
    if direction.len() != embedding.len() || embedding.is_empty() {
        return None;
    }
    let mut dot = 0.0;
    let mut norm = 0.0;
    for (d, e) in direction.iter().zip(embedding) {
        if !e.is_finite() {
            return None;
        }
        let e = f64::from(*e);
        dot += d * e;
        norm += e * e;
    }
    if norm <= 0.0 {
        return None;
    }
    Some(clamp01((dot / norm.sqrt()).abs()))
}

fn top_terms(delta: &BTreeMap<String, u32>, corpus: &Corpus, n: usize) -> Vec<String> {
    let mut weighted: Vec<(f64, &String)> = delta
        .iter()
        .map(|(t, c)| ((1.0 + f64::from(*c).ln()) * corpus.idf(t), t))
        .collect();
    weighted.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(y.1)));
    weighted
        .into_iter()
        .take(n)
        .map(|(_, t)| t.clone())
        .collect()
}

// ---- comparison ---------------------------------------------------------------

/// Compares two texts. `corpus` supplies IDF weights (a two-document corpus
/// of the texts themselves when absent); `embeddings` are the two sides'
/// semantic vectors, if the caller has them.
pub fn compare(
    before: &str,
    after: &str,
    policy: &TextPolicy,
    corpus: Option<&Corpus>,
    embeddings: Option<(&[f32], &[f32])>,
) -> TextDiff {
    let lex = Lexicon::new(policy);
    let a = analyze_with(before, &lex);
    let mut b = analyze_with(after, &lex);
    align_typos(&a, &mut b, &lex);
    let local;
    let corpus = match corpus {
        Some(c) if !c.is_empty() => c,
        _ => {
            let mut c = Corpus::new();
            c.add_terms(a.counts.keys());
            c.add_terms(b.counts.keys());
            local = c;
            &local
        }
    };
    compare_analyses(&a, &b, policy, corpus, embeddings)
}

/// How far a text of `longest` characters is into the long-text regime:
/// 0 up to `short` characters, 1 from twice that, linear in between (so the
/// lexical formula never jumps at a length).
fn long_text_share(longest: usize, short: usize) -> f64 {
    if longest <= short {
        0.0
    } else if short == 0 || longest >= short.saturating_mul(2) || longest > MAX_CHAR_DISTANCE_CHARS
    {
        1.0
    } else {
        (longest - short) as f64 / short as f64
    }
}

/// The evidence counts the magnitude combines with the base distance.
#[derive(Debug, Clone, Copy, Default)]
struct Evidence {
    structural_edits: f64,
    negation_flips: u32,
    modal_shifts: u32,
    numbers_changed: u32,
}

/// `m = noisy-OR(base, wᵢ·eᵢ)`: decreasing in the similarity `1 − base`
/// and increasing in every piece of evidence, so a strictly larger edit
/// never scores lower.
///
/// No extra discount ties the evidence to the lexical similarity: noisy-OR
/// already scales each piece's marginal effect by the similarity that is
/// left (`∂m/∂eᵢ ∝ 1 − base`), which is the overlap correction between a
/// rewrite and its structural signals; a second, similarity-dependent
/// factor made `m` fall when more words were edited.
fn combine(base: f64, ev: &Evidence, policy: &TextPolicy) -> f64 {
    let saturate = |rate: f64, n: f64| 1.0 - (-rate * n.max(0.0)).exp();
    noisy_or([
        base,
        policy.structure_weight * saturate(0.5, ev.structural_edits),
        policy.negation_weight * saturate(0.7, f64::from(ev.negation_flips)),
        policy.modal_weight * saturate(0.4, f64::from(ev.modal_shifts)),
        policy.number_weight * saturate(0.5, f64::from(ev.numbers_changed)),
    ])
}

/// Negation flips, modal shifts and changed numbers inside reworded passages.
fn passage_evidence(a: &Analysis, b: &Analysis, pairing: &PassagePairing) -> (u32, u32, u32) {
    let (mut flips, mut shifts, mut numbers) = (0u32, 0u32, 0u32);
    for &(i, j) in &pairing.reworded {
        let (x, y) = (&a.passages[i], &b.passages[j]);
        // A flip is a term on both sides whose negated count changed; a
        // negation that came with a new term belongs to that rewording.
        for t in x.terms.keys().filter(|t| y.terms.contains_key(*t)) {
            let negated = |p: &Passage| p.negated.get(t).copied().unwrap_or(0);
            flips = flips.saturating_add(negated(x).abs_diff(negated(y)));
        }
        let changed = x
            .modal
            .iter()
            .filter(|(t, s)| y.modal.get(*t).is_some_and(|o| o != *s))
            .count();
        shifts = shifts.saturating_add(to_u32(changed));
        let (na, nb) = multiset_diff(&x.numbers, &y.numbers);
        numbers = numbers.saturating_add(na.max(nb));
    }
    (flips, shifts, numbers)
}

/// Every sentence's tokens, sorted.
fn sorted_segments(x: &Analysis) -> Vec<&str> {
    let mut v: Vec<&str> = x.segments.iter().map(String::as_str).collect();
    v.sort_unstable();
    v
}

/// Same sentences, list items (with their check state) and headings, in
/// another order or layout.
fn is_reorder(a: &Analysis, b: &Analysis) -> bool {
    a.normalized != b.normalized
        && a.segments.len() == b.segments.len()
        && a.headings == b.headings
        && item_states(a) == item_states(b)
        && sorted_segments(a) == sorted_segments(b)
}

pub(crate) fn compare_analyses(
    a: &Analysis,
    b: &Analysis,
    policy: &TextPolicy,
    corpus: &Corpus,
    embeddings: Option<(&[f32], &[f32])>,
) -> TextDiff {
    let chars_before = a.char_count();
    let chars_after = b.char_count();
    let both_empty = a.normalized.is_empty() && b.normalized.is_empty();
    let longest = chars_before.max(chars_after);
    let long_share = long_text_share(longest, policy.short_text_chars);

    let char_similarity = (long_share < 1.0).then(|| {
        if longest == 0 {
            1.0
        } else {
            1.0 - levenshtein(&a.normalized, &b.normalized) as f64 / longest as f64
        }
    });

    let token_jaccard = key_jaccard(&a.counts, &b.counts);
    let shingle_similarity = jaccard(
        &shingles(&a.raw, policy.shingle_size),
        &shingles(&b.raw, policy.shingle_size),
    );
    let tfidf_cosine = match (a.counts.is_empty(), b.counts.is_empty()) {
        (true, true) => 1.0,
        (true, false) | (false, true) => 0.0,
        (false, false) => cosine(&tfidf(&a.counts, corpus), &tfidf(&b.counts, corpus)),
    };
    let lexical_similarity = if both_empty {
        1.0
    } else {
        let lerp = |s: f64, l: f64| s + (l - s) * long_share;
        let (sc, st, ss) = SHORT_TEXT_WEIGHTS;
        let (lc, lt, ls) = LONG_TEXT_WEIGHTS;
        lerp(sc, lc) * char_similarity.unwrap_or(0.0)
            + lerp(st, lt) * tfidf_cosine
            + lerp(ss, ls) * shingle_similarity
    };
    let reordered = is_reorder(a, b);
    let mut lexical_distance = clamp01(1.0 - lexical_similarity);
    if reordered {
        lexical_distance = lexical_distance.min(REORDER_LEXICAL_DISTANCE);
    }

    let embedding_cosine = embeddings.and_then(|(x, y)| embedding_cosine(x, y));
    let base = match embedding_cosine {
        Some(cos) => {
            let semantic = clamp01((1.0 - cos) / (1.0 - policy.embedding_floor));
            policy.embedding_weight * semantic + (1.0 - policy.embedding_weight) * lexical_distance
        }
        None => lexical_distance,
    };

    let structure = diff_structure(a, b);
    let pairing = pair_passages(a, b);
    let (negation_flips, modal_shifts, numbers_changed) = passage_evidence(a, b, &pairing);
    let magnitude = if both_empty {
        0.0
    } else {
        combine(
            base,
            &Evidence {
                structural_edits: structure.edits(),
                negation_flips,
                modal_shifts,
                numbers_changed,
            },
            policy,
        )
    };

    let added = count_delta(&a.counts, &b.counts);
    let removed = count_delta(&b.counts, &a.counts);
    TextDiff {
        magnitude,
        lexical_similarity: clamp01(lexical_similarity),
        char_similarity,
        token_jaccard,
        shingle_similarity,
        tfidf_cosine,
        embedding_cosine,
        structure,
        negation_flips,
        modal_shifts,
        numbers_changed,
        added_terms: top_terms(&added, corpus, 8),
        removed_terms: top_terms(&removed, corpus, 8),
        chars_before,
        chars_after,
        low_information: a.term_count().min(b.term_count()) < 4,
        passages_added: to_u32(pairing.added.len()),
        passages_removed: to_u32(pairing.removed.len()),
        passages_modified: to_u32(pairing.reworded.len()),
        reordered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> TextPolicy {
        TextPolicy::default()
    }

    /// The counts-only path must give exactly the full analysis's counts:
    /// the engine uses it for context documents and titles.
    #[test]
    fn term_counts_equal_the_full_analysis() {
        let pol = p();
        let lex = Lexicon::new(&pol);
        let texts = [
            "",
            "Hello world",
            "# Heading here\n- [ ] item one\n- [x] item two\n> quoted text\n\
             ```\ncode block\n```\nPlain. Second sentence! Third?",
            "Хэрэглэгч нэвтэрч чадахгүй. 10,000 users at 1.5x speed",
            "can't won't shouldn't; it isn't",
            "a b c",
            "  * nested\n    1. numbered item\n~~~\nfenced\n~~~\n## Done",
            "Must not support SSO. Should be optional?",
        ];
        for t in texts {
            assert_eq!(term_counts(t, &lex), analyze(t, &pol).counts, "{t:?}");
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
        #[test]
        fn term_counts_match_the_full_analysis(
            lines in proptest::collection::vec(
                "(# |## |- |- \\[ \\] |- \\[x\\] |> |```|1\\. |  )?[a-zA-Zа-яөүё0-9 ,.;:!?'-]{0,48}",
                0..8,
            )
        ) {
            let text = lines.join("\n");
            let pol = p();
            let lex = Lexicon::new(&pol);
            proptest::prop_assert_eq!(term_counts(&text, &lex), analyze(&text, &pol).counts);
        }
    }

    #[test]
    fn tokens_and_stems() {
        assert_eq!(
            raw_tokens("Don't block 1.5 users, 10,000!"),
            vec!["do", "not", "block", "1.5", "users", "10000"]
        );
        assert_eq!(stem("blocked"), "block");
        assert_eq!(stem("blocking"), "block");
        assert_eq!(stem("stories"), "story");
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("https"), "https");
        assert_eq!(stem("bugs"), "bug");
        assert_eq!(stem("нууц"), "нууц");
        assert_eq!(raw_tokens("Нууц үг сэргээх"), vec!["нууц", "үг", "сэргээх"]);
    }

    #[test]
    fn identical_texts_have_zero_magnitude() {
        let t = "Implement OAuth login.\n\n- [ ] Google provider\n- [ ] GitHub provider";
        let d = compare(t, t, &p(), None, None);
        assert!(d.magnitude < 1e-9, "{d:?}");
        assert_eq!(d.structure.edits(), 0.0);
    }

    #[test]
    fn typo_is_tiny_rewrite_is_large() {
        let base = "Users can reset their password by email. The link expires after 24 hours.";
        let typo = "Users can reset their pasword by email. The link expires after 24 hours.";
        let rewrite =
            "Replace password login entirely with OAuth single sign-on through Google and GitHub.";
        let small = compare(base, typo, &p(), None, None).magnitude;
        let large = compare(base, rewrite, &p(), None, None).magnitude;
        assert!(small < 0.2, "typo magnitude {small}");
        assert!(large > 0.6, "rewrite magnitude {large}");
    }

    #[test]
    fn negation_flip_is_caught() {
        let d = compare(
            "The service must support SAML single sign-on for enterprise tenants.",
            "The service must not support SAML single sign-on for enterprise tenants.",
            &p(),
            None,
            None,
        );
        assert_eq!(d.negation_flips, 1);
        assert_eq!(d.passages_modified, 1, "{d:?}");
        assert!(d.magnitude > 0.4, "magnitude {}", d.magnitude);
    }

    #[test]
    fn modal_and_number_changes() {
        let d = compare(
            "The API should handle 100 requests per second.",
            "The API must handle 1000 requests per second.",
            &p(),
            None,
            None,
        );
        assert_eq!(d.modal_shifts, 1);
        assert_eq!(d.numbers_changed, 1);
        assert!(d.magnitude > 0.4);
    }

    #[test]
    fn checklist_diff() {
        let before = "## Acceptance\n- [ ] Login with Google\n- [ ] Login with GitHub\n- [ ] Reset password by email";
        let after = "## Acceptance\n- [x] Login with Google\n- [ ] Login with GitHub accounts\n- [ ] Audit log of logins";
        let d = compare(before, after, &p(), None, None);
        assert_eq!(d.structure.items_checked, 1);
        assert_eq!(d.structure.items_modified, 1, "{:?}", d.structure);
        assert_eq!(d.structure.items_removed, 1);
        assert_eq!(d.structure.items_added, 1);
    }

    #[test]
    fn ticking_boxes_is_not_scope() {
        let before = "- [ ] Write the migration\n- [ ] Backfill the table";
        let after = "- [x] Write the migration\n- [x] Backfill the table";
        let d = compare(before, after, &p(), None, None);
        assert_eq!(d.structure.edits(), 0.0);
        assert!(!d.reordered, "a tick is progress, not a reorder");
        assert!(d.magnitude < 0.15, "magnitude {}", d.magnitude);
    }

    #[test]
    fn embeddings_blend() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        let d = compare(
            "same words here today",
            "same words here today!",
            &p(),
            None,
            Some((&a, &b)),
        );
        assert_eq!(d.embedding_cosine, Some(0.0));
        assert!(d.magnitude > 0.5);
        assert!(embedding_cosine(&a, &[1.0, 0.0]).is_none());
        assert!(embedding_cosine(&[0.0, 0.0], &[0.0, 0.0]).is_none());
        assert!(embedding_cosine(&[f32::NAN], &[1.0]).is_none());
    }

    #[test]
    fn typos_are_patch_sized() {
        for (a, b) in [
            ("Checkout", "Checkuot"),
            (
                "Implement the payment gateway",
                "Implement the paymnet gateway",
            ),
            (
                "Migrate users to the new database",
                "Migrate users to teh new database",
            ),
            ("Send the welcome email", "Send the welcome emaul"),
        ] {
            let d = compare(a, b, &p(), None, None);
            assert!(d.magnitude < 0.2, "{a} → {b}: {}", d.magnitude);
            assert!(
                d.added_terms.is_empty() && d.removed_terms.is_empty(),
                "{d:?}"
            );
        }
        // Real word changes are not typos.
        for (a, b) in [
            ("Enable login", "Disable login"),
            ("Import users", "Export users"),
            ("Login page", "Logout page"),
        ] {
            let d = compare(a, b, &p(), None, None);
            assert!(d.magnitude > 0.3, "{a} → {b}: {}", d.magnitude);
        }
        let mut budget = usize::MAX;
        assert_eq!(
            osa_within(&['t', 'e', 'h'], &['t', 'h', 'e'], 1, &mut budget),
            Some(1)
        );
    }

    #[test]
    fn levenshtein_known_values() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("same", "same"), 0);
        assert_eq!(levenshtein("сайн", "сайхан"), 2);
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(compare("", "", &p(), None, None).magnitude, 0.0);
        assert!(compare("", "Something new to do here", &p(), None, None).magnitude > 0.9);
        assert!(compare("Fix the login", "Fix  the   login", &p(), None, None).magnitude < 1e-9);
    }

    #[test]
    fn delta_relevance() {
        let pol = p();
        let a = analyze(
            "Support email and password login. Password reset by email.",
            &pol,
        );
        let b = analyze("Support OAuth login with Google.", &pol);
        let mut corpus = Corpus::new();
        let child_reset = analyze("Password reset flow: send a reset email", &pol);
        let child_oauth = analyze("Google OAuth provider integration", &pol);
        let child_css = analyze("Fix footer CSS on mobile", &pol);
        for an in [&a, &b, &child_reset, &child_oauth, &child_css] {
            corpus.add_terms(an.counts.keys());
        }
        let delta = text_delta(&a, &b, &corpus, None);
        let reset = delta.overlap(&tfidf(&child_reset.counts, &corpus));
        let oauth = delta.overlap(&tfidf(&child_oauth.counts, &corpus));
        let css = delta.overlap(&tfidf(&child_css.counts, &corpus));
        assert!(reset.removed > oauth.removed, "{reset:?} {oauth:?}");
        assert!(oauth.added > reset.added);
        assert!(css.max() < 0.05);
    }

    #[test]
    fn reworded_passages_are_modified_not_removed() {
        let pol = p();
        let a = analyze(
            "Export the monthly invoices as PDF. Sessions expire after 30 days.",
            &pol,
        );
        let b = analyze(
            "Show a usage dashboard with charts. Sessions expire after 7 days.",
            &pol,
        );
        let session = analyze(
            "Session expiry: expire sessions after 30 days of inactivity",
            &pol,
        );
        let mut corpus = Corpus::new();
        for an in [&a, &b, &session] {
            corpus.add_terms(an.counts.keys());
        }
        let delta = text_delta(&a, &b, &corpus, None);
        assert_eq!(delta.counts(), (1, 1, 1), "{delta:?}");
        let o = delta.overlap(&tfidf(&session.counts, &corpus));
        assert!(o.modified > 0.5, "{o:?}");
        assert!(o.modified > o.removed);
        assert_eq!(
            split_sentences("Ship v2.0 today. Then rest!"),
            vec!["Ship v2.0 today.", " Then rest!"]
        );
    }

    // ---- regression tests for the review findings ---------------------------

    fn word(i: usize) -> String {
        // Distinct alphabetic words (no digits, so they are not numbers).
        let mut s = String::from("w");
        let mut n = i;
        loop {
            s.push(char::from(b'a' + u8::try_from(n % 26).unwrap_or(0)));
            n /= 26;
            if n == 0 {
                break;
            }
        }
        s
    }

    /// The wall-clock bound of the timing tests. They guard against
    /// quadratic work, which takes minutes, so an unoptimised build on a
    /// busy machine gets a wide margin.
    const TIME_LIMIT_SECS: f64 = if cfg!(debug_assertions) { 10.0 } else { 1.0 };

    /// Runs `f`, returning its result and wall time. Timing tests hold a
    /// lock so they do not compete with each other for the CPU.
    fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = std::time::Instant::now();
        let r = f();
        let secs = t.elapsed().as_secs_f64();
        eprintln!("timed: {secs:.3}s");
        (r, secs)
    }

    /// What `fields.rs` does for one text field.
    fn assess_text(before: &str, after: &str) -> (TextDiff, TextDelta) {
        let pol = p();
        let lex = Lexicon::new(&pol);
        let a = analyze_with(before, &lex);
        let mut b = analyze_with(after, &lex);
        align_typos(&a, &mut b, &lex);
        let mut corpus = Corpus::new();
        corpus.add_terms(a.counts.keys());
        corpus.add_terms(b.counts.keys());
        let diff = compare_analyses(&a, &b, &pol, &corpus, None);
        let delta = text_delta(&a, &b, &corpus, None);
        (diff, delta)
    }

    const CHECKOUT: &str = "Customers pay at checkout with a card or PayPal.\n\n\
        ## Acceptance criteria\n\
        - [ ] Credit card payments\n\
        - [ ] PayPal payments\n\
        - [ ] Saved payment methods";

    /// #5: a negation or a number inside an *added* sentence or item is part
    /// of the addition, not a flip or a changed number.
    #[test]
    fn added_negations_and_numbers_are_additions() {
        let cases = [
            (
                "\n\nGuests can check out without creating an account.",
                "\n\nGuests can check out by creating an account.",
            ),
            (
                "\n- [ ] No extra fees for card payments",
                "\n- [ ] Lower fees for card payments",
            ),
            (
                "\n- [ ] Use OAuth 2.0 for the PayPal connection",
                "\n- [ ] Use OAuth for the PayPal connection",
            ),
        ];
        for (with, without) in cases {
            let d = compare(CHECKOUT, &format!("{CHECKOUT}{with}"), &p(), None, None);
            let control = compare(CHECKOUT, &format!("{CHECKOUT}{without}"), &p(), None, None);
            assert_eq!(
                (d.negation_flips, d.numbers_changed),
                (0, 0),
                "{with}: {d:?}"
            );
            assert_eq!(d.structure.items_removed, 0, "{with}");
            assert_eq!(d.passages_added, 1, "{with}: {d:?}");
            assert!(
                (d.magnitude - control.magnitude).abs() < 0.05,
                "{with}: {} vs control {}",
                d.magnitude,
                control.magnitude
            );
            assert!(d.magnitude < 0.5, "{with}: additive, not breaking: {d:?}");
        }
        // "3-D" is a name, not the number 3.
        let d = compare("Support 3-D Secure", "Support 3D Secure", &p(), None, None);
        assert_eq!(d.numbers_changed, 0, "{d:?}");
        assert!(d.magnitude < 0.1, "{d:?}");
        assert_eq!(
            raw_tokens("3-D Secure, UTF-8, a 10-day trial on 2026-11-07"),
            [
                "3d", "secure", "utf8", "a", "10", "day", "trial", "on", "2026", "11", "07"
            ]
        );
        // Inside a reworded sentence, flips and numbers still count.
        let d = compare(
            "Sessions expire after 30 days. Admins can export reports.",
            "Sessions expire after 7 days. Admins cannot export reports.",
            &p(),
            None,
            None,
        );
        assert_eq!((d.negation_flips, d.numbers_changed), (1, 1), "{d:?}");
        // Changing which verb is negated is a rewording, not a flip.
        let d = compare(
            "Do not log passwords.",
            "Do not store passwords.",
            &p(),
            None,
            None,
        );
        assert_eq!(d.negation_flips, 0, "{d:?}");
    }

    /// #7: polarity and requirement strength are part of a passage's
    /// identity, so the flipped sentence is a modified passage that overlaps
    /// the child it concerns (and not an unrelated one).
    #[test]
    fn negated_or_strengthened_passages_reach_their_child() {
        let pol = p();
        for (before, after) in [
            (
                "The service must support SAML single sign-on for enterprise tenants.",
                "The service must not support SAML single sign-on for enterprise tenants.",
            ),
            (
                "The service should support SAML single sign-on for enterprise tenants.",
                "The service must support SAML single sign-on for enterprise tenants.",
            ),
        ] {
            let a = analyze(before, &pol);
            let b = analyze(after, &pol);
            let saml = analyze("SAML single sign-on for enterprise tenants", &pol);
            let css = analyze("Fix footer CSS on mobile", &pol);
            let mut corpus = Corpus::new();
            for an in [&a, &b, &saml, &css] {
                corpus.add_terms(an.counts.keys());
            }
            let delta = text_delta(&a, &b, &corpus, None);
            assert_eq!(delta.counts(), (0, 0, 1), "{after}");
            let o_saml = delta.overlap(&tfidf(&saml.counts, &corpus));
            let o_css = delta.overlap(&tfidf(&css.counts, &corpus));
            assert!(o_saml.modified > 0.5, "{after}: {o_saml:?}");
            assert!(o_css.max() < 0.05, "{after}: {o_css:?}");
            let d = compare(before, after, &pol, None, None);
            assert_eq!(d.passages_modified, 1, "{d:?}");
            assert_eq!(d.negation_flips + d.modal_shifts, 1, "{d:?}");
        }
    }

    /// #8: units, versions, protocols and engines are not typos.
    #[test]
    fn opposite_words_are_not_typos_of_each_other() {
        let typo = |a: &str, b: &str| {
            let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
            let mut budget = usize::MAX;
            is_typo_of(&a, &b, &mut budget).is_some() || is_typo_of(&b, &a, &mut budget).is_some()
        };
        for (a, b) in [
            ("increase", "decrease"),
            ("internal", "external"),
            ("encryption", "decryption"),
            ("maximize", "minimize"),
            ("maximum", "minimum"),
            ("interstate", "intrastate"),
            ("hyperthermia", "hypothermia"),
            ("microservice", "macroservice"),
            ("include", "exclude"),
            ("review", "preview"),
            ("affect", "effect"),
        ] {
            assert!(!typo(a, b), "{a} / {b}");
        }
        for (a, b) in [
            ("authentication", "authentcation"),
            ("authentication", "authenticaton"),
            ("receive", "recieve"),
            ("environment", "enviroment"),
            ("deployment", "deploymnet"),
            ("deployment", "depolyment"),
            ("deployment", "edployment"),
            ("separate", "seperate"),
            ("committed", "commited"),
            ("authentication", "authentocatian"),
        ] {
            assert!(typo(a, b), "{a} / {b}");
        }
    }

    #[test]
    fn units_versions_and_affixes_are_not_typos() {
        for (before, after, numeric) in [
            (
                "p95 latency must stay under 200ms for checkout",
                "p95 latency must stay under 500ms for checkout",
                true,
            ),
            (
                "Serve the public API over http only",
                "Serve the public API over https only",
                false,
            ),
            (
                "Migrate the login flow to oauth1 tokens",
                "Migrate the login flow to oauth2 tokens",
                true,
            ),
            (
                "Store sessions in mysql for the billing service",
                "Store sessions in mssql for the billing service",
                false,
            ),
            (
                "Ship the export API as v2.0 for partners",
                "Ship the export API as v3.0 for partners",
                true,
            ),
            (
                "Make the import worker sync for large files",
                "Make the import worker async for large files",
                false,
            ),
            (
                "Support ipv4 addresses in the allowlist",
                "Support ipv6 addresses in the allowlist",
                true,
            ),
            (
                "Show a review of the order before payment",
                "Show a preview of the order before payment",
                false,
            ),
        ] {
            let d = compare(before, after, &p(), None, None);
            assert!(
                !d.added_terms.is_empty() && !d.removed_terms.is_empty(),
                "{after}: {d:?}"
            );
            assert!(d.magnitude > 0.15, "{after}: {}", d.magnitude);
            if numeric {
                assert_eq!(d.numbers_changed, 1, "{after}: {d:?}");
            }
        }
        // Spaced units behave the same as glued ones.
        let glued = compare(
            "p95 latency must stay under 200ms for checkout",
            "p95 latency must stay under 500ms for checkout",
            &p(),
            None,
            None,
        );
        let spaced = compare(
            "p95 latency must stay under 200 ms for checkout",
            "p95 latency must stay under 500 ms for checkout",
            &p(),
            None,
            None,
        );
        assert!(
            (glued.magnitude - spaced.magnitude).abs() < 0.1,
            "{glued:?} {spaced:?}"
        );
    }

    fn osa_reference(a: &[char], b: &[char]) -> usize {
        let (n, m) = (a.len(), b.len());
        let mut d = vec![vec![0usize; m + 1]; n + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in d[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=n {
            for j in 1..=m {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1)
                    .min(d[i][j - 1] + 1)
                    .min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[n][m]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// #9: the banded, early-exit OSA agrees with the full matrix.
        #[test]
        fn banded_osa_matches_full_matrix(a in "[abc]{0,12}", b in "[abc]{0,12}", max in 0usize..7) {
            let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
            let full = osa_reference(&a, &b);
            let mut budget = usize::MAX;
            proptest::prop_assert_eq!(osa_within(&a, &b, max, &mut budget), (full <= max).then_some(full));
        }
    }

    /// #9: long tokens are never typo-aligned, so a 16 KB (or 1 MiB) token
    /// costs linear time and memory.
    #[test]
    fn long_tokens_are_cheap() {
        for n in [16_000usize, 64_000, 1 << 20] {
            let before = format!("Spec {} end", "a".repeat(n));
            let after = format!("Spec {}b end", "a".repeat(n - 1));
            let ((d, _), secs) = timed(|| assess_text(&before, &after));
            assert!(secs < TIME_LIMIT_SECS, "token of {n} chars took {secs:.3}s");
            assert!(d.magnitude > 0.0 && d.magnitude <= 1.0);
        }
        // Many medium tokens, each edited once.
        let mut before = String::new();
        let mut after = String::new();
        for i in 0..500 {
            let base: String = word(i).repeat(100 / word(i).len() + 1)[..100].to_owned();
            before.push_str(&base);
            before.push(' ');
            after.push_str(&base[..99]);
            after.push_str("z ");
        }
        let (_, secs) = timed(|| assess_text(&before, &after));
        assert!(
            secs < TIME_LIMIT_SECS,
            "500 edited 100-char tokens took {secs:.3}s"
        );
        // The cell budget bounds a typo pass whatever the input.
        let words: Vec<String> = (0..500)
            .map(|i| {
                format!(
                    "{}{}",
                    "q".repeat(39),
                    char::from(b'a' + u8::try_from(i % 26).unwrap_or(0))
                )
            })
            .collect();
        let (_, secs) = timed(|| typo_map(words.iter(), words.iter().rev()));
        assert!(secs < TIME_LIMIT_SECS, "{secs:.3}s");
    }

    /// #10: exact matching is hashed and fuzzy pairing is bounded: the
    /// 175 KB checklist and the 67 KB paragraph finish well under a second.
    #[test]
    fn long_lists_pair_in_near_linear_time() {
        let n = 8000;
        let before = (0..n)
            .map(|i| format!("- [ ] task alpha {}", word(i)))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (0..n)
            .map(|i| format!("- [ ] task alpha {}x", word(i)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(before.len() > 170_000);
        let ((d, delta), secs) = timed(|| assess_text(&before, &after));
        assert!(secs < TIME_LIMIT_SECS, "{n} reworded items took {secs:.3}s");
        let s = d.structure;
        assert_eq!(s.items_before, 8000);
        assert!(s.items_modified > 7000, "{s:?}");
        assert_eq!(s.items_removed, s.items_added);
        let (added, removed, modified) = delta.counts();
        assert_eq!(
            (added, removed),
            (to_u32(added) as usize, to_u32(removed) as usize)
        );
        assert!(modified > 7000, "{added} {removed} {modified}");

        let before = (0..4000)
            .map(|i| format!("Task alpha {}.", word(i)))
            .collect::<Vec<_>>()
            .join(" ");
        let after = (0..4000)
            .map(|i| format!("Task alpha {}x.", word(i)))
            .collect::<Vec<_>>()
            .join(" ");
        let ((d, _), secs) = timed(|| assess_text(&before, &after));
        assert!(
            secs < TIME_LIMIT_SECS,
            "4000 reworded sentences took {secs:.3}s"
        );
        assert!(d.passages_modified > 3500, "{d:?}");

        // A tail edit of 10 000 items (exact matching is hashed).
        let items: Vec<String> = (0..10_000)
            .map(|i| format!("- [ ] {} {}", word(i), word(i + 7)))
            .collect();
        let before = items.join("\n");
        let mut tail = items.clone();
        if let Some(last) = tail.last_mut() {
            *last = "- [ ] completely different".to_owned();
        }
        let after = tail.join("\n");
        let ((d, _), secs) = timed(|| assess_text(&before, &after));
        assert!(
            secs < TIME_LIMIT_SECS,
            "tail edit of 10 000 items took {secs:.3}s"
        );
        assert_eq!((d.structure.items_removed, d.structure.items_added), (1, 1));
    }

    /// #10: the exact and fuzzy pairing still find moved, reworded items in
    /// a list too large to score every pair.
    #[test]
    fn large_pairing_finds_moved_items_by_rare_terms() {
        let mut before: Vec<String> = (0..1000)
            .map(|i| format!("- [ ] {} {} {}", word(i), word(i + 5000), word(i + 9000)))
            .collect();
        let mut after: Vec<String> = (0..1000)
            .map(|i| format!("- [ ] {} {} {}x", word(i), word(i + 5000), word(i + 9000)))
            .collect();
        // The first item moves to the end and is reworded.
        before.push("- [ ] Reconcile ledgers nightly for finance".to_owned());
        after.insert(0, "- [ ] Reconcile ledgers weekly for finance".to_owned());
        let d = compare(&before.join("\n"), &after.join("\n"), &p(), None, None);
        assert_eq!(d.structure.items_modified, 1001, "{:?}", d.structure);
        assert_eq!(d.structure.items_removed, 0);
    }

    /// #29: a one-word reword of a short item is a modified item, not a
    /// removal plus an addition (which would read as breaking).
    #[test]
    fn short_items_reworded_are_modified() {
        for (before, after) in [
            ("- [ ] Login with Google", "- [ ] Sign-in with Google"),
            (
                "- [ ] Login with Google accounts",
                "- [ ] Sign-in with Google accounts",
            ),
            ("- [ ] Limit 100", "- [ ] Limit 1000"),
        ] {
            let (d, delta) = assess_text(before, after);
            assert_eq!(d.structure.items_removed, 0, "{before}: {:?}", d.structure);
            assert_eq!(d.structure.items_modified, 1, "{before}");
            assert_eq!(d.passages_modified, 1, "{before}");
            assert_eq!(delta.counts(), (0, 0, 1), "{before}");
        }
        // Unrelated short items stay a removal and an addition.
        let d = compare("- [ ] Add tests", "- [ ] Add docs", &p(), None, None);
        assert_eq!((d.structure.items_removed, d.structure.items_added), (1, 1));
    }

    /// #35: the inverted index gives the same overlap as scoring every
    /// passage, and its cost follows the target's terms.
    #[test]
    fn overlap_index_matches_brute_force_and_scales() {
        let pol = p();
        let mut corpus = Corpus::new();
        let texts: Vec<Analysis> = (0..40)
            .map(|i| {
                analyze(
                    &format!(
                        "{} {} {} {}",
                        word(i % 7),
                        word(i % 11),
                        word(i % 13 + 20),
                        word(i)
                    ),
                    &pol,
                )
            })
            .collect();
        for t in &texts {
            corpus.add_terms(t.counts.keys());
        }
        let vecs: Vec<SparseVec> = texts.iter().map(|t| tfidf(&t.counts, &corpus)).collect();
        let (removed, rest) = vecs.split_at(10);
        let (added, modified) = rest.split_at(15);
        let delta = TextDelta::new(added.to_vec(), removed.to_vec(), modified.to_vec(), None);
        let best =
            |ps: &[SparseVec], d: &SparseVec| ps.iter().map(|p| cosine(p, d)).fold(0.0, f64::max);
        for d in &vecs {
            let o = delta.overlap(d);
            assert!((o.removed - best(removed, d)).abs() < 1e-12);
            assert!((o.added - best(added, d)).abs() < 1e-12);
            assert!((o.modified - best(modified, d)).abs() < 1e-12);
        }
        // A rewritten 2 000-sentence parent against 50 000 children.
        let before = (0..2000)
            .map(|i| format!("Deliver part {}.", word(i)))
            .collect::<Vec<_>>()
            .join(" ");
        let after = (0..2000)
            .map(|i| format!("Replace with {}.", word(i + 100_000)))
            .collect::<Vec<_>>()
            .join(" ");
        let (_, delta) = assess_text(&before, &after);
        assert_eq!(delta.counts(), (2000, 2000, 0));
        let lex = Lexicon::new(&pol);
        let kids: Vec<SparseVec> = (0..50_000)
            .map(|i| {
                let mut v = SparseVec::new();
                v.insert("kid".to_owned(), 0.6);
                if let Some(t) = lex.term(&word(i)) {
                    v.insert(t, 0.8);
                }
                v
            })
            .collect();
        let (hits, secs) = timed(|| kids.iter().filter(|k| delta.overlap(k).max() > 0.0).count());
        assert!(secs < TIME_LIMIT_SECS, "50 000 overlaps took {secs:.3}s");
        let changed: BTreeSet<String> = (0..2000)
            .flat_map(|i| [lex.term(&word(i)), lex.term(&word(i + 100_000))])
            .flatten()
            .collect();
        let expected = (0..50_000)
            .filter(|i| lex.term(&word(*i)).is_some_and(|t| changed.contains(&t)))
            .count();
        assert_eq!(hits, expected);
        assert!(hits >= 2000);
    }

    /// #49: a pure reorder is near zero; reordering words inside a sentence
    /// or swapping modal verbs between sentences is not a reorder.
    #[test]
    fn pure_reorders_are_near_zero() {
        for (before, after) in [
            (
                "- Card number field\n- Expiry field\n- CVC field",
                "- CVC field\n- Card number field\n- Expiry field",
            ),
            (
                "Customers pay at checkout. Payment methods are listed by country. Receipts go out by email.\n\n- [ ] Credit card payments support 3-D Secure\n- [ ] PayPal payments\n- [ ] Saved payment methods",
                "Receipts go out by email. Customers pay at checkout. Payment methods are listed by country.\n\n- [ ] Saved payment methods\n- [ ] Credit card payments support 3-D Secure\n- [ ] PayPal payments",
            ),
            ("## Scope\nShip it.", "### Scope\n\nShip it!"),
        ] {
            let d = compare(before, after, &p(), None, None);
            assert!(d.reordered, "{after}: {d:?}");
            assert!(
                d.magnitude <= REORDER_LEXICAL_DISTANCE + 1e-12,
                "{after}: {}",
                d.magnitude
            );
        }
        for (before, after) in [
            ("Users block admins.", "Admins block users."),
            (
                "Admins must approve refunds. Agents should log calls.",
                "Admins should approve refunds. Agents must log calls.",
            ),
            (
                "- [x] Write docs\n- [ ] Ship",
                "- [ ] Write docs\n- [x] Ship",
            ),
            ("- Write docs\n- Ship", "Write docs\n- Ship"),
        ] {
            let d = compare(before, after, &p(), None, None);
            assert!(!d.reordered, "{after}: {d:?}");
        }
        let d = compare(
            "Admins must approve refunds. Agents should log calls.",
            "Admins should approve refunds. Agents must log calls.",
            &p(),
            None,
            None,
        );
        assert_eq!(d.modal_shifts, 2, "{d:?}");
    }

    /// #50: the character-similarity weight fades in smoothly, so adding a
    /// few characters around 512 does not jump the magnitude.
    #[test]
    fn lexical_formula_is_continuous_in_length() {
        let core_b = "Generate the monthly invoice PDF with customer address, tax breakdown and payment terms.";
        let core_a = "Generate the weekly receipt PDF with customer address, vat breakdown and refund terms.";
        let filler = "Additional context sentence about billing operations follows here. ";
        let mut prev: Option<(usize, f64)> = None;
        for target in (440..1100).step_by(3) {
            let mut b = format!("{core_b} {}", filler.repeat(target / filler.len() + 1));
            b.truncate(target);
            let b = b.trim_end().to_owned();
            let a = b.replacen(core_b, core_a, 1);
            let m = compare(&b, &a, &p(), None, None).magnitude;
            if let Some((t, pm)) = prev {
                assert!(
                    (m - pm).abs() < 0.01,
                    "len {t} → {target}: {pm:.4} → {m:.4}"
                );
            }
            prev = Some((target, m));
        }
        assert!((long_text_share(512, 512) - 0.0).abs() < 1e-12);
        assert!((long_text_share(768, 512) - 0.5).abs() < 1e-12);
        assert!((long_text_share(1024, 512) - 1.0).abs() < 1e-12);
        assert!((long_text_share(10, 0) - 1.0).abs() < 1e-12);
    }

    /// #54: with an L-independent evidence weight, the magnitude falls with
    /// the lexical similarity and rises with every piece of evidence.
    #[test]
    fn magnitude_is_monotone_in_similarity_and_evidence() {
        let pol = p();
        for edits in [0.0, 1.0, 3.0, 6.0, 12.0] {
            for flips in [0, 1, 3] {
                let ev = Evidence {
                    structural_edits: edits,
                    negation_flips: flips,
                    modal_shifts: 1,
                    numbers_changed: 2,
                };
                let mut last = f64::INFINITY;
                for step in 0..=100 {
                    let similarity = f64::from(step) / 100.0;
                    let m = combine(1.0 - similarity, &ev, &pol);
                    assert!(
                        m <= last + 1e-12,
                        "edits {edits} flips {flips} L {similarity}"
                    );
                    last = m;
                }
                let more = Evidence {
                    structural_edits: edits + 1.0,
                    ..ev
                };
                assert!(combine(0.3, &more, &pol) >= combine(0.3, &ev, &pol));
            }
        }
        // The finding's formula check: edits = 6, L = 0.8 vs 1.0.
        let ev = Evidence {
            structural_edits: 6.0,
            ..Evidence::default()
        };
        assert!(combine(0.2, &ev, &pol) >= combine(0.0, &ev, &pol));

        // The finding's repro: three negation flips and five removed items,
        // then extra word edits — the magnitude must not fall.
        let intro = "Finance reconciles both ledgers nightly during the shadow period.";
        let items: Vec<String> = (0..10).map(|i| format!("- [ ] Item{i} done")).collect();
        let neg_before = "The importer must accept partial files. The importer must accept empty rows. The importer must accept legacy headers.";
        let neg_after = "The importer must not accept partial files. The importer must not accept empty rows. The importer must not accept legacy headers.";
        let before = format!("{intro} {neg_before}\n{}", items.join("\n"));
        let afters = [
            format!("{intro} {neg_after}\n{}", items[5..].join("\n")),
            format!(
                "{} {neg_after}\n{}",
                intro.replace("nightly", "weekly"),
                items[5..].join("\n")
            ),
            format!(
                "{} {neg_after}\n{}",
                intro.replace(
                    "nightly during the shadow period",
                    "weekly after the cutover window"
                ),
                items[5..].join("\n")
            ),
        ];
        let ms: Vec<f64> = afters
            .iter()
            .map(|a| compare(&before, a, &pol, None, None).magnitude)
            .collect();
        let d = compare(&before, &afters[0], &pol, None, None);
        assert_eq!(
            (d.negation_flips, d.structure.items_removed),
            (3, 5),
            "{d:?}"
        );
        assert!(ms.windows(2).all(|w| w[1] + 1e-12 >= w[0]), "{ms:?}");
    }

    const VOCAB: [&str; 24] = [
        "invoice", "export", "ledger", "payment", "gateway", "refund", "customer", "tenant",
        "billing", "account", "report", "widget", "search", "filter", "upload", "storage",
        "backup", "region", "latency", "cache", "queue", "worker", "schema", "token",
    ];

    /// Two sentences and five list items over distinct words; the words at
    /// `edited` positions are replaced by same-length words in another
    /// alphabet (so every lexical measure can only move one way).
    fn edited_text(edited: &BTreeSet<usize>) -> String {
        let w = |k: usize| -> String {
            if edited.contains(&k) {
                let len = VOCAB[k].chars().count();
                let base = u32::from('а');
                let k32 = u32::try_from(k).unwrap_or(0);
                (0..len)
                    .map(|i| {
                        let off = match i {
                            0 => k32 % 32,
                            1 => k32 / 32,
                            _ => 6,
                        };
                        char::from_u32(base + off).unwrap_or('ж')
                    })
                    .collect()
            } else {
                VOCAB[k].to_owned()
            }
        };
        let words = |r: std::ops::Range<usize>| r.map(w).collect::<Vec<_>>().join(" ");
        format!(
            "{}. {}.\n\n- [ ] {}\n- [ ] {}\n- [ ] {}\n- [ ] {}\n- [ ] {}",
            words(0..5),
            words(5..10),
            words(10..13),
            words(13..15),
            words(15..18),
            words(18..20),
            words(20..24),
        )
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// #54: applying a strict superset of edits never lowers the magnitude.
        #[test]
        fn magnitude_never_decreases_for_a_superset_of_edits(
            more in proptest::collection::btree_set(0usize..24, 1..=24),
            keep in proptest::collection::vec(proptest::bool::ANY, 24),
            drop in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let dropped = *drop.get(&more.iter().copied().collect::<Vec<_>>());
            let fewer: BTreeSet<usize> = more.iter().copied().filter(|k| keep[*k] && *k != dropped).collect();
            let base = edited_text(&BTreeSet::new());
            let small = compare(&base, &edited_text(&fewer), &p(), None, None);
            let large = compare(&base, &edited_text(&more), &p(), None, None);
            proptest::prop_assert!(
                large.magnitude + 1e-12 >= small.magnitude,
                "{fewer:?} → {}, {more:?} → {}", small.magnitude, large.magnitude
            );
        }
    }
}
