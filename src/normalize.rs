use std::iter::FusedIterator;

use derive_more::Deref;
use rapidhash::{HashMapExt, RapidHashMap};
use thiserror::Error;

use crate::dict::RuleIdVec;
use crate::typed_vec::TypedVec;
use crate::vocab::TokenIdVec;
use crate::{Dictionary, RuleId, TokenId, bpe_with_heap_last_merge};

#[derive(Clone, Debug, Error)]
#[non_exhaustive]
pub enum NormalizedDictBuildError {
    #[error("multiple atomic token sequences for token {token_id} ({seq_a:?} vs {seq_b:?})")]
    MultipleAtomicTokenSeq {
        token_id: TokenId,
        seq_a: Vec<TokenId>,
        seq_b: Vec<TokenId>,
    },
    #[error("improper rules for token {token_id} (proper result: {proper:?})")]
    ImproperDict {
        token_id: TokenId,
        proper: Vec<TokenId>,
    },
}

#[derive(Clone, Debug, Deref)]
pub struct NormalizedDict {
    #[deref]
    dict: Dictionary,
    pub(crate) priorities: TypedVec<TokenId, RuleId>,
    #[cfg(test)]
    pub(crate) canonical_rules: RapidHashMap<(TokenId, TokenId), RuleId>,
}

pub(crate) const ATOMIC_TOKEN_PRIORITY: RuleId = {
    let mut priority = RuleId::MAX;
    *priority.inner_mut() = (priority.inner() >> 1) + 1;
    priority
};

#[inline(always)]
fn to_atomic_token_id(rule_id: RuleId) -> TokenId {
    debug_assert!(rule_id >= ATOMIC_TOKEN_PRIORITY);
    TokenId::new((rule_id - ATOMIC_TOKEN_PRIORITY).inner())
}

impl NormalizedDict {
    pub fn new<F: FnMut(&Dictionary, TokenId, &[u8]) -> bool>(
        dict: Dictionary,
        mut is_atomic: F,
    ) -> Result<Self, NormalizedDictBuildError> {
        let capacity = dict.num_of_tokens();
        let mut priorities = TypedVec::new_with(RuleId::MAX, capacity);
        let mut canonical_rules = RapidHashMap::with_capacity(capacity.as_usize());

        let mut atomic_seqs = TypedVec::new_with(TokenIdVec::new(), capacity);

        for (token_id, priority) in priorities.enumerate_mut() {
            let token = &dict[token_id];
            if token.is_empty() {
                continue;
            }
            if is_atomic(&dict, token_id, token) {
                atomic_seqs[token_id].push(token_id);
                debug_assert!(token_id.as_usize() < ATOMIC_TOKEN_PRIORITY.as_usize());
                let mut p = ATOMIC_TOKEN_PRIORITY;
                *p.inner_mut() += token_id.inner();
                *priority = p;
            }
        }

        let mut token_to_rules = TypedVec::new_with(RuleIdVec::new(), capacity);
        for (rule_id, rule) in dict.rules.enumerate() {
            token_to_rules[rule.merged].push(rule_id);
        }
        for token_id in {
            let mut order: Vec<_> = dict.tokens.keys().collect();
            order.sort_by_key(|&i| dict[i].len());
            order
        } {
            for &rule_id in &token_to_rules[token_id] {
                let rule = &dict[rule_id];
                if atomic_seqs[rule.pre].is_empty() || atomic_seqs[rule.suc].is_empty() {
                    continue;
                }
                let mut seq = atomic_seqs[rule.pre].clone();
                seq.extend_from_slice(&atomic_seqs[rule.suc]);
                let slot = &mut atomic_seqs[token_id];
                if !slot.is_empty() && *slot != seq {
                    return Err(NormalizedDictBuildError::MultipleAtomicTokenSeq {
                        token_id,
                        seq_a: slot.to_vec(),
                        seq_b: seq.to_vec(),
                    });
                }
                *slot = seq;
            }
        }
        drop(token_to_rules);

        let mut validation = TypedVec::new_with(false, dict.num_of_rules());
        for (token_id, seq) in atomic_seqs.enumerate() {
            if seq.is_empty() {
                continue;
            }
            let improper = bpe_with_heap_last_merge::<true>(&dict, seq.to_vec());
            if improper.0 != vec![token_id] {
                continue;
            }
            let proper = bpe_with_heap_last_merge::<false>(&dict, seq.to_vec());
            if proper != improper {
                return Err(NormalizedDictBuildError::ImproperDict {
                    token_id,
                    proper: proper.0,
                });
            }
            if let Some(last_rule_id) = proper.1 {
                validation[last_rule_id] = true;
            }
        }
        drop(atomic_seqs);

        'outer: for (id, rule) in dict.rules.enumerate() {
            let mut left = priorities[rule.pre];
            let mut right = priorities[rule.suc];
            if priorities[rule.merged] != RuleId::MAX || left == RuleId::MAX || right == RuleId::MAX
            {
                continue;
            }
            while left < ATOMIC_TOKEN_PRIORITY || right < ATOMIC_TOKEN_PRIORITY {
                let (u, v): (TokenId, TokenId);
                if left == right {
                    u = dict[left].suc;
                    v = dict[right].pre;
                } else if left >= ATOMIC_TOKEN_PRIORITY {
                    u = to_atomic_token_id(left);
                    v = dict[right].pre;
                    debug_assert_eq!(left, priorities[u]);
                } else if right >= ATOMIC_TOKEN_PRIORITY {
                    u = dict[left].suc;
                    v = to_atomic_token_id(right);
                    debug_assert_eq!(right, priorities[v]);
                } else if left > right {
                    u = dict[left].suc;
                    v = dict[right].merged;
                    debug_assert_eq!(right, priorities[v]);
                } else {
                    u = dict[left].merged;
                    v = dict[right].pre;
                    debug_assert_eq!(left, priorities[u]);
                }
                if let Some(&mid) = canonical_rules.get(&(u, v)) {
                    debug_assert!(priorities[u] >= ATOMIC_TOKEN_PRIORITY || mid > priorities[u]);
                    debug_assert!(priorities[v] >= ATOMIC_TOKEN_PRIORITY || mid > priorities[v]);
                    if left == right || right == priorities[v] {
                        if mid < left {
                            continue 'outer;
                        }
                    } else if mid <= right {
                        continue 'outer;
                    }
                }
                if left < ATOMIC_TOKEN_PRIORITY {
                    left = priorities[u];
                }
                if right < ATOMIC_TOKEN_PRIORITY {
                    right = priorities[v];
                }
                debug_assert_ne!(left, RuleId::MAX);
                debug_assert_ne!(right, RuleId::MAX);
            }
            priorities[rule.merged] = id;
            let res = canonical_rules.insert((rule.pre, rule.suc), id);
            debug_assert!(res.is_none());
            debug_assert!(validation[id]);
            validation[id] = false;
        }

        debug_assert!(validation.into_iter().all(|i| !i));

        Ok(Self {
            dict,
            priorities,
            #[cfg(test)]
            canonical_rules,
        })
    }

    #[inline]
    pub fn new_in_bytes(dict: Dictionary) -> Result<Self, NormalizedDictBuildError> {
        Self::new(dict, |_, _, b| b.len() == 1)
    }

    #[inline]
    pub fn new_in_utf8(dict: Dictionary) -> Result<Self, NormalizedDictBuildError> {
        Self::new(dict, |_, _, b| {
            if b.len() > 4 {
                return false;
            }
            std::str::from_utf8(b).is_ok_and(|s| s.chars().count() == 1)
        })
    }

    #[inline(always)]
    pub fn priority(&self, token_id: TokenId) -> RuleId {
        self.priorities
            .get(token_id)
            .copied()
            .unwrap_or(RuleId::MAX)
    }

    #[inline(always)]
    pub fn is_atomic(&self, token_id: TokenId) -> bool {
        self.is_canonical(token_id) && self.priorities[token_id] >= ATOMIC_TOKEN_PRIORITY
    }

    #[inline(always)]
    pub fn is_canonical(&self, token_id: TokenId) -> bool {
        self.priority(token_id) != RuleId::MAX
    }

    #[inline(always)]
    pub fn iter_canonical_or_empty_tokens(
        &self,
    ) -> impl DoubleEndedIterator<Item = &[u8]> + ExactSizeIterator + FusedIterator {
        self.tokens.enumerate().map(|(token_id, bytes)| {
            if self.is_canonical(token_id) {
                bytes.as_ref()
            } else {
                &[]
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{bytes_into_tokens, utf8_into_tokens};
    use crate::{
        Dictionary, NormalizedDict, NormalizedDictBuildError, RuleId, Vocab, bpe_with_heap,
    };

    fn build_dict<T: AsRef<[u8]>, R: IntoIterator<Item = (T, T)>>(
        vocab: &Vocab,
        rules: R,
    ) -> Dictionary {
        Dictionary::new_from_token_pair(vocab.clone(), rules).unwrap()
    }

    fn build_in_bytes(dict: &Dictionary) -> Option<NormalizedDict> {
        let dict = match NormalizedDict::new_in_bytes(dict.clone()) {
            Ok(dict) => dict,
            Err(NormalizedDictBuildError::ImproperDict { .. }) => {
                return None;
            }
            Err(e) => {
                dbg!(e);
                unreachable!();
            }
        };
        for rule in &dict.rules {
            let token_id = rule.merged;
            assert!(!dict.is_atomic(token_id));
            let seq = &dict[token_id];
            let res = bpe_with_heap::<false>(&dict, bytes_into_tokens(&dict, seq, 0usize));
            assert!(dict.is_canonical(token_id) ^ (res != vec![token_id]));
        }
        Some(dict)
    }

    fn build_in_utf8(dict: &Dictionary) -> Option<NormalizedDict> {
        let dict = match NormalizedDict::new_in_utf8(dict.clone()) {
            Ok(dict) => dict,
            Err(NormalizedDictBuildError::ImproperDict { .. }) => {
                return None;
            }
            Err(e) => {
                dbg!(e);
                unreachable!();
            }
        };
        for rule in &dict.rules {
            let token_id = rule.merged;
            let seq = match std::str::from_utf8(&dict[token_id]) {
                Ok(seq) => seq,
                Err(_) => {
                    assert!(!dict.is_canonical(token_id));
                    continue;
                }
            };
            assert!(!dict.is_atomic(token_id));
            let res = bpe_with_heap::<false>(&dict, utf8_into_tokens(&dict, seq, 0usize));
            assert!(dict.is_canonical(token_id) ^ (res != vec![token_id]));
        }
        Some(dict)
    }

    fn canonical_rules<R: IntoIterator<Item = u32>>(dict: &NormalizedDict, rules: R) {
        let mut rules: Vec<_> = rules.into_iter().map(RuleId::new).collect();
        rules.sort();
        let mut expected: Vec<_> = dict.canonical_rules.values().copied().collect();
        expected.sort();
        assert_eq!(rules, expected);
    }

    fn build_and_test_rules<R: IntoIterator<Item = u32> + Clone>(dict: &Dictionary, rules: R) {
        if let Some(normalized) = build_in_bytes(dict) {
            canonical_rules(&normalized, rules.clone());
        }
        if let Some(normalized) = build_in_utf8(dict) {
            canonical_rules(&normalized, rules);
        }
    }

    #[test]
    fn test_normalized_dict() {
        let vocab = Vocab::new([
            b"" as &[_],
            b"a",
            b"b",
            b"c",
            b"d",
            b"cd",
            b"bcd",
            b"abcd",
            "你".as_bytes(),
            "好".as_bytes(),
            "呀".as_bytes(),
            "你好".as_bytes(),
            "你好呀".as_bytes(),
            "好你".as_bytes(),
            b"\xe4",
            b"\xbd",
            b"\xa0",
            b"\xbd\xa0",
            b"aa",
            b"aaa",
            b"aaaa",
            b"aaaaa",
        ])
        .unwrap();

        let dict = build_dict(&vocab, [("c", "d"), ("b", "cd"), ("a", "bcd")]);
        build_and_test_rules(&dict, [0, 1, 2]);

        let dict = build_dict(
            &vocab,
            [(b"\xbd" as &[_], b"\xa0" as &[_]), (b"\xe4", b"\xbd\xa0")],
        );
        let normalized = build_in_bytes(&dict).unwrap();
        canonical_rules(&normalized, [0, 1]);

        let dict = build_dict(&vocab, [("aa", "a"), ("a", "a")]);
        build_and_test_rules(&dict, [1]);

        let dict = build_dict(&vocab, [("a", "aa"), ("a", "a")]);
        build_and_test_rules(&dict, [1]);

        let dict = build_dict(&vocab, [("a", "a"), ("aa", "a")]);
        build_and_test_rules(&dict, [0, 1]);

        let dict = build_dict(&vocab, [("a", "a"), ("a", "aa")]);
        build_and_test_rules(&dict, [0]);

        let dict = build_dict(
            &vocab,
            [
                ("a", "a"),
                ("aa", "a"),
                ("a", "aa"),
                ("aa", "aa"),
                ("a", "aaa"),
                ("aaa", "a"),
            ],
        );
        build_and_test_rules(&dict, [0, 1, 3]);

        let dict = build_dict(&vocab, [("a", "a"), ("aa", "a"), ("aaa", "a")]);
        build_and_test_rules(&dict, [0, 1]);

        let dict = build_dict(&vocab, [("a", "a"), ("aa", "a"), ("aa", "aa")]);
        build_and_test_rules(&dict, [0, 1, 2]);
        let dict = build_dict(&vocab, [("a", "a"), ("aa", "aa"), ("aa", "a")]);
        build_and_test_rules(&dict, [0, 1, 2]);

        let dict = build_dict(
            &vocab,
            [
                ("a", "a"),
                ("aa", "aa"),
                ("aa", "a"),
                ("aaa", "aa"),
                ("aa", "aaa"),
                ("aaaa", "a"),
            ],
        );
        build_and_test_rules(&dict, [0, 1, 2, 5]);

        let dict = build_dict(
            &vocab,
            [
                ("a", "a"),
                ("aa", "a"),
                ("aa", "aa"),
                ("aaa", "aa"),
                ("aa", "aaa"),
                ("aaaa", "a"),
            ],
        );
        build_and_test_rules(&dict, [0, 1, 2, 4]);

        let dict = build_dict(&vocab, [("你", "好"), ("你好", "呀")]);
        let normalized = build_in_utf8(&dict).unwrap();
        canonical_rules(&normalized, [0, 1]);
        let dict = build_dict(&vocab, [("你", "好"), ("你好", "呀"), ("好", "你")]);
        let normalized = build_in_utf8(&dict).unwrap();
        canonical_rules(&normalized, [0, 1, 2]);
        let dict = build_dict(&vocab, [("你", "好"), ("好", "你"), ("你好", "呀")]);
        let normalized = build_in_utf8(&dict).unwrap();
        canonical_rules(&normalized, [0, 1, 2]);
        let dict = build_dict(&vocab, [("好", "你"), ("你", "好"), ("你好", "呀")]);
        let normalized = build_in_utf8(&dict).unwrap();
        canonical_rules(&normalized, [0, 1, 2]);
        let dict = build_dict(&vocab, [("你好", "呀"), ("你", "好"), ("好", "你")]);
        assert!(build_in_utf8(&dict).is_none());
        let dict = build_dict(&vocab, [("你好", "呀"), ("好", "你"), ("你", "好")]);
        assert!(build_in_utf8(&dict).is_none());
        let dict = build_dict(&vocab, [("好", "你"), ("你好", "呀"), ("你", "好")]);
        assert!(build_in_utf8(&dict).is_none());

        let vocab = Vocab::new([
            b"" as &[_],
            b"a",
            b"abc",
            b"abcde",
            b"abcdef",
            b"b",
            b"ba",
            b"bc",
            b"bcdef",
            b"c",
            b"cd",
            b"cde",
            b"cdefg",
            b"d",
            b"de",
            b"def",
            b"e",
            b"ef",
            b"efg",
            b"f",
            b"g",
        ])
        .unwrap();
        let dict = build_dict(
            &vocab,
            [
                ("b", "c"),
                ("e", "f"),
                ("d", "e"),
                ("c", "d"),
                ("d", "ef"),
                ("b", "a"),
                ("a", "bc"),
                ("abc", "de"),
                ("abc", "def"),
                ("bc", "def"),
                ("c", "de"),
                ("ef", "g"),
                ("cd", "efg"),
            ],
        );
        build_and_test_rules(&dict, 0..13);
        let dict = build_dict(
            &vocab,
            [
                ("b", "c"),
                ("e", "f"),
                ("d", "e"),
                ("c", "d"),
                ("d", "ef"),
                ("a", "bc"),
                ("b", "a"),
                ("abc", "de"),
                ("abc", "def"),
                ("bc", "def"),
                ("c", "de"),
                ("ef", "g"),
                ("cd", "efg"),
            ],
        );
        build_and_test_rules(&dict, 0..13);
    }

    #[test]
    fn test_normalized_dict_invalid() {
        let dict = Dictionary::new_from_id_pair(
            Vocab::new([b"a" as &[_], b"aa"]).unwrap(),
            [(0usize, 0usize)],
        )
        .unwrap();
        let res = NormalizedDict::new(dict.clone(), |_, _, b| b.len() == 1);
        assert!(res.is_ok());
        let res = NormalizedDict::new(dict, |_, _, _| true);
        assert!(res.is_err());
    }
}
