use regex::bytes::RegexSet;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use regex::Regex;

use crate::MarkStyle;

pub type PatternId = usize;

#[derive(Debug, Clone)]
pub struct StyledChar {
    pub c: char,
    pub matches: Option<Vec<PatternId>>, // Option to avoid allocations
    pub style: MarkStyle,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum MatchType {
    BigWord,
    SmallWord,
    Text,
    Regex,
}

impl MatchType {
    pub fn delimiter(&self) -> &'static str {
        match self {
            MatchType::BigWord => " \t",
            MatchType::SmallWord => " \t:.,\"';()[]{}<>=+-*/&|^~!@#$%?",  // see also build_re
            MatchType::Text => "",
            MatchType::Regex => "",
        }
    }

    pub fn build_re(&self, pattern: &str) -> String {
        match self {
            MatchType::BigWord => {
                let charclass = "[\t ]";
                format!("(?:{}|^|\n)({})(?:$|\n|{})", charclass, regex::escape(pattern), charclass)
            }
            MatchType::SmallWord => {
                let charclass = "[\t :.,\"';()\\[\\]{}<>=+\\-*/&|^~!@#$%?]";
                format!("(?:{}|^|\n)({})(?:$|\n|{})", charclass, regex::escape(pattern), charclass)
            }
            MatchType::Text => {
                format!("({})", regex::escape(pattern))
            }
            MatchType::Regex => {
                // TODO: validate pattern
                format!(r"({})", pattern)
            }
        }
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum PatternMode {
    Tagging,
    Hiding,
    Marking,
    Search,
}

#[derive(Debug)]
pub struct Pattern {
    pub pattern: String,
    pub style: MarkStyle,
    pub mode: PatternMode,
    pub match_type: MatchType,
    re: Regex,
}

#[derive(Debug)]
pub struct PatternSet {
    pub default_style: MarkStyle,
    patterns: BTreeMap<PatternId, Pattern>,
    pub seq: PatternId,
    pub tagged_re: RegexSet,
    pub search_re: RegexSet,
    pub hidden_re: RegexSet,
}

impl PatternSet {
    pub fn new(default_style: MarkStyle) -> Self {
        PatternSet {
            patterns: BTreeMap::new(),
            tagged_re: RegexSet::new(&[""; 0]).unwrap(),
            search_re: RegexSet::new(&[""; 0]).unwrap(),
            hidden_re: RegexSet::new(&[""; 0]).unwrap(),
            seq: 1,
            default_style,
        }
    }

    fn rebuild_re(&mut self) {
        self.seq += 1;
        let tagged_patterns = self.patterns
            .values()
            .filter(|p| p.mode == PatternMode::Tagging)
            .map(|p| p.match_type.build_re(&p.pattern));
        self.tagged_re = RegexSet::new(tagged_patterns).unwrap();

        let search_patterns = self.patterns
            .values()
            .filter(|p| p.mode == PatternMode::Search)
            .map(|p| p.match_type.build_re(&p.pattern));
        self.search_re = RegexSet::new(search_patterns).unwrap();

        let hidden_patterns = self.patterns.values()
            .filter(|p| p.mode == PatternMode::Hiding)
            .map(|p| p.match_type.build_re(&p.pattern));
        self.hidden_re = RegexSet::new(hidden_patterns).unwrap();
    }

    pub fn add(&mut self, pattern: &str, match_type: MatchType, style: MarkStyle,
        mode: PatternMode) -> PatternId
    {
        let id = self.seq;
        let re = Regex::new(&match_type.build_re(pattern)).unwrap();
        let pat = Pattern {
            pattern: pattern.to_string(),
            style,
            mode,
            match_type,
            re,
        };
        self.patterns.insert(id, pat);
        self.rebuild_re();

        id
    }

    pub fn remove(&mut self, id: PatternId) {
        let old = self.patterns.remove(&id);
        assert!(old.is_some());
        self.rebuild_re();
    }

    pub fn get(&self, id: PatternId) -> &Pattern {
        self.patterns.get(&id).unwrap()
    }

    pub fn with<F>(&mut self, id: PatternId, f: F) 
        where F: FnOnce(&mut Pattern)
    {
        let pattern = self.patterns.get_mut(&id).unwrap();
        f(pattern);
        pattern.re = Regex::new(&pattern.match_type.build_re(&pattern.pattern)).unwrap();
        self.rebuild_re();
    }

    pub fn is_tagging(&self, id: PatternId) -> bool {
        self.get(id).mode == PatternMode::Tagging
    }

    pub fn is_hiding(&self, id: PatternId) -> bool {
        self.get(id).mode == PatternMode::Hiding
    }

    pub fn get_tagged_re(&self) -> RegexSet {
        self.tagged_re.clone()
    }

    pub fn get_search_re(&self) -> RegexSet {
        self.search_re.clone()
    }

    pub fn get_hidden_re(&self) -> RegexSet {
        self.hidden_re.clone()
    }

    pub fn process_line(&self, line: &str) -> (Vec<StyledChar>, Vec<PatternId>) {
        let mut pline = Vec::new();
        let mut matches = BTreeSet::new();

        for c in line.chars() {
            pline.push(StyledChar {
                c,
                style: self.default_style.clone(),
                matches: None,
            });
        }
        if pline.last().map(|c| c.c) == Some('\n') {
            pline.pop();
        }
        for (&id, pattern) in self.patterns.iter() {
            for c in pattern.re.captures_iter(line) {
                let m = c.get(1).unwrap();
                for i in m.start() .. m.end() {
                    pline[i].style = pattern.style.clone();
                    if let Some(ref mut matches) = pline[i].matches {
                        matches.push(id);
                    } else {
                        pline[i].matches = Some(vec![id]);
                    }
                    matches.insert(id);
                }
            }
        }

        let matches = matches.into_iter().collect();
        (pline, matches)
    }
}
