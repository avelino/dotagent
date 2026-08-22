//! Ranking for recall. Pure: no workspace, no clock, no IO.
//!
//! The first version of recall asked whether the whole message was a
//! substring of a stored fact. It practically never was — "e aí, o que ficou
//! pendente do databricks?" is not a substring of anything — so every recall
//! degenerated into "the last N blocks written", regardless of subject.
//!
//! Ranking here is deliberately lexical and explainable rather than
//! semantic. The store holds text, not vectors, and an assistant states
//! recall results as fact: a near-miss surfaced confidently is worse than a
//! miss. So a fact only competes if it shares a real term with the query,
//! and among those, recency and reinforcement break the tie.

/// A fact as the ranker sees it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub text: &'a str,
    pub topics: &'a [String],
    /// Days between the fact's journal and today. Negative values (a fact
    /// dated in the future) are treated as today.
    pub age_days: i64,
    /// How many times this fact was written. 1 for a fact stated once.
    pub seen: u32,
}

/// Weight of lexical agreement, the dominant term: a fact that matches every
/// query term should outrank a fresher fact that matches one.
const MATCH_WEIGHT: f32 = 3.0;
/// Weight of a topic hit, above a body hit — the query naming a subject is a
/// stronger signal than the same word appearing mid-sentence.
const TOPIC_BONUS: f32 = 0.5;
/// Ceiling of the recency term.
const RECENCY_WEIGHT: f32 = 1.0;
/// Days for recency to decay to ~37%. Two months: long enough that a
/// preference stated in June still competes in August, short enough that
/// today's correction wins.
const RECENCY_HALFLIFE_DAYS: f32 = 60.0;
/// Weight of repetition. Logarithmic — the fifth restatement of a fact means
/// much less than the second.
const REINFORCEMENT_WEIGHT: f32 = 0.4;

/// Terms worth matching on, from arbitrary text.
///
/// Everything non-alphanumeric splits, so `databricks-cost-daily` yields
/// `databricks`, `cost`, `daily` and a query for "databricks" finds it.
/// Short tokens and stopwords are dropped: matching on "de" or "the" would
/// make every fact a hit and flatten the ranking into pure recency, which is
/// the failure this module exists to fix.
///
/// Two normalizations run on every token, applied identically to the query
/// and to the stored fact — the point is that both sides land on the same
/// form, not that the form is a real word:
///
/// - **Accents fold.** Portuguese gets typed both ways, and a topic slug is
///   whatever the agent coined. A question about "pendência" has to find a
///   fact tagged `pendencias`.
/// - **A trailing plural `s` is dropped** when enough word remains. "custos"
///   and "custo" are the same subject; treating them as different terms is a
///   miss for no reason. Irregular plurals are left alone — a stemmer that
///   guesses is how "reunião" starts matching "reunir".
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        let token = normalize(raw);
        if token.chars().count() < 3 || is_stopword(&token) {
            continue;
        }
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

/// Shortest form of a token that the query and the fact can both reach.
fn normalize(raw: &str) -> String {
    let folded: String = raw.to_lowercase().chars().map(fold_accent).collect();
    // 4 keeps "mes" and "gas" whole while still folding "custos" onto
    // "custo": below that the stem stops being recognizable as the word.
    match folded.strip_suffix('s') {
        Some(stem) if stem.chars().count() >= 4 => stem.to_string(),
        _ => folded,
    }
}

/// Latin letters that differ from their base only by a diacritic.
///
/// A hand-written table rather than full Unicode normalization: these are
/// the letters this store actually holds, and the alternative is a
/// dependency to strip combining marks from text that is Portuguese and
/// English.
fn fold_accent(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        'ý' | 'ÿ' => 'y',
        other => other,
    }
}

/// Score a candidate against pre-extracted query terms.
///
/// Returns `0.0` when nothing lexical matches, which callers read as "not a
/// hit" — an empty query is the caller's business, not a match of everything.
pub fn score(query_terms: &[String], candidate: &Candidate) -> f32 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let body = terms(candidate.text);
    let topics: Vec<String> = candidate
        .topics
        .iter()
        .flat_map(|t| terms(t))
        .collect::<Vec<_>>();

    let mut hits = 0.0f32;
    for term in query_terms {
        let in_body = body.contains(term);
        let in_topic = topics.contains(term);
        if in_body || in_topic {
            hits += 1.0;
        }
        if in_topic {
            hits += TOPIC_BONUS;
        }
    }
    if hits == 0.0 {
        return 0.0;
    }
    let ratio = (hits / query_terms.len() as f32).min(1.0 + TOPIC_BONUS);
    MATCH_WEIGHT * ratio + recency(candidate.age_days) + reinforcement(candidate.seen)
}

/// Score with no query: how a fact ranks when the caller wants "what is
/// recent and well-established" rather than "what is relevant".
pub fn baseline(candidate: &Candidate) -> f32 {
    recency(candidate.age_days) + reinforcement(candidate.seen)
}

fn recency(age_days: i64) -> f32 {
    let age = age_days.max(0) as f32;
    RECENCY_WEIGHT * (-age / RECENCY_HALFLIFE_DAYS).exp()
}

fn reinforcement(seen: u32) -> f32 {
    REINFORCEMENT_WEIGHT * (seen.max(1) as f32).ln()
}

/// Words that carry no retrieval signal, in the two languages this store
/// actually holds. Not exhaustive by design — a stopword list that grows
/// unbounded starts eating real query terms.
fn is_stopword(token: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        // pt
        "para", "pra", "pelo", "pela", "com", "sem", "mas", "que", "porque", "quando", "onde",
        "como", "qual", "quais", "isso", "isto", "aquilo", "esse", "essa", "este", "esta", "dos",
        "das", "nos", "nas", "uma", "uns", "umas", "foi", "era", "ser", "sao", "são", "tem", "ter",
        "tudo", "todo", "toda", "mais", "menos", "muito", "voce", "você", "meu", "minha", "seu",
        "sua", "não", "nao", "sim", "aqui", "ali", "hoje", "agora", "ainda", "então", "entao",
        // en
        "the", "and", "for", "with", "that", "this", "from", "was", "were", "are", "you", "your",
        "our", "his", "her", "their", "what", "when", "where", "which", "who", "how", "why", "all",
        "any", "can", "did", "does", "has", "have", "had", "not", "but", "its", "it's", "about",
        "into", "than", "then", "them", "they", "there", "here", "now", "just", "some", "such",
        "only", "over", "also", "been", "being", "will", "would", "should", "could",
    ];
    STOPWORDS.contains(&token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate<'a>(text: &'a str, topics: &'a [String]) -> Candidate<'a> {
        Candidate {
            text,
            topics,
            age_days: 0,
            seen: 1,
        }
    }

    #[test]
    fn terms_split_on_punctuation_so_compound_names_are_findable() {
        // The whole reason recall used to miss: "databricks" never was a
        // substring match against a sentence, and an exact-token split would
        // have kept `databricks-cost-daily` whole.
        //
        // The trailing `s` is stemmed off, which is fine and invisible: the
        // query goes through the same normalization, so both sides land on
        // `databrick` and still meet.
        assert_eq!(
            terms("databricks-cost-daily quebrou"),
            vec!["databrick", "cost", "daily", "quebrou"]
        );
        assert_eq!(
            terms("databricks"),
            terms("databricks-cost-daily")[..1].to_vec()
        );
    }

    #[test]
    fn terms_drop_stopwords_and_short_tokens() {
        // "o", "eu" go for length; "que", "para", "com", "isso" for being
        // stopwords. What survives is what a query is actually about.
        assert_eq!(
            terms("o que eu tenho para fazer com isso"),
            vec!["tenho", "fazer"]
        );
    }

    #[test]
    fn terms_fold_accents_so_the_same_word_matches_either_spelling() {
        // Real miss this fixes: a question about "pendência" found nothing,
        // while the fact sat there tagged `pendencias`.
        assert_eq!(terms("pendência"), terms("pendencia"));
        assert_eq!(terms("reunião"), terms("reuniao"));
        assert_eq!(terms("serviço"), terms("servico"));
    }

    #[test]
    fn terms_fold_a_plural_onto_its_singular() {
        assert_eq!(terms("custos"), terms("custo"));
        assert_eq!(terms("pendências"), terms("pendencia"));
        assert_eq!(terms("agents"), terms("agent"));
    }

    #[test]
    fn a_short_word_ending_in_s_is_left_whole() {
        // Stripping here would leave a stem too short to mean anything.
        assert_eq!(terms("mes"), vec!["mes"]);
        assert_eq!(terms("gas"), vec!["gas"]);
    }

    #[test]
    fn an_accented_question_finds_the_unaccented_topic() {
        let q = terms("tem alguma pendência com prazo?");
        let topics = vec!["pendencias".to_string()];
        assert!(
            score(
                &q,
                &candidate("TODO até 2026-08-26: relatório pro Mario", &topics)
            ) > 0.0
        );
    }

    #[test]
    fn terms_are_deduplicated() {
        assert_eq!(terms("rust rust rust"), vec!["rust"]);
    }

    #[test]
    fn terms_are_case_insensitive() {
        assert_eq!(terms("Roam RESEARCH"), vec!["roam", "research"]);
    }

    #[test]
    fn a_shared_term_scores_and_an_unrelated_one_does_not() {
        let q = terms("databricks");
        assert!(score(&q, &candidate("databricks-cost-daily quebrou", &[])) > 0.0);
        assert_eq!(score(&q, &candidate("prefere reunião de manhã", &[])), 0.0);
    }

    #[test]
    fn a_natural_question_finds_the_fact_it_is_about() {
        // The exact shape that returned nothing before: a conversational
        // question against a terse stored fact.
        let q = terms("e aí, o que ficou pendente do databricks?");
        assert!(
            score(
                &q,
                &candidate("databricks-cost-daily quebrou em 07-13", &[])
            ) > 0.0
        );
    }

    #[test]
    fn matching_more_terms_outranks_matching_fewer() {
        let q = terms("custo databricks");
        let both = score(&q, &candidate("custo do databricks subiu", &[]));
        let one = score(&q, &candidate("databricks roda de madrugada", &[]));
        assert!(both > one, "both={both} one={one}");
    }

    #[test]
    fn a_topic_hit_outranks_the_same_word_in_the_body() {
        let q = terms("dotagent");
        let topics = vec!["dotagent".to_string()];
        let tagged = score(&q, &candidate("roda no launchd", &topics));
        let body = score(&q, &candidate("dotagent roda no launchd", &[]));
        assert!(tagged > body, "tagged={tagged} body={body}");
    }

    #[test]
    fn relevance_beats_recency() {
        // The bug this replaces: recency was the *only* signal, so an
        // unrelated fact written today outranked the answer from March.
        let q = terms("databricks");
        let old_hit = score(
            &q,
            &Candidate {
                text: "databricks-cost-daily quebrou",
                topics: &[],
                age_days: 180,
                seen: 1,
            },
        );
        let fresh_miss = score(
            &q,
            &Candidate {
                text: "almoçou no japonês",
                topics: &[],
                age_days: 0,
                seen: 1,
            },
        );
        assert!(old_hit > fresh_miss, "old={old_hit} fresh={fresh_miss}");
    }

    #[test]
    fn among_equal_matches_the_fresher_one_wins() {
        let q = terms("reunião");
        let fresh = score(
            &q,
            &Candidate {
                text: "prefere reunião depois das 14h",
                topics: &[],
                age_days: 1,
                seen: 1,
            },
        );
        let stale = score(
            &q,
            &Candidate {
                text: "prefere reunião depois das 14h",
                topics: &[],
                age_days: 400,
                seen: 1,
            },
        );
        assert!(fresh > stale, "fresh={fresh} stale={stale}");
    }

    #[test]
    fn among_equal_matches_the_repeated_one_wins() {
        let q = terms("reunião");
        let repeated = score(
            &q,
            &Candidate {
                text: "prefere reunião depois das 14h",
                topics: &[],
                age_days: 0,
                seen: 5,
            },
        );
        let once = score(&q, &candidate("prefere reunião depois das 14h", &[]));
        assert!(repeated > once, "repeated={repeated} once={once}");
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        // "everything matches" is the caller's decision (see `baseline`),
        // never a side effect of an empty needle.
        assert_eq!(score(&[], &candidate("qualquer fato", &[])), 0.0);
    }

    #[test]
    fn a_query_of_only_stopwords_matches_nothing() {
        assert_eq!(
            score(&terms("o que é isso"), &candidate("um fato", &[])),
            0.0
        );
    }

    #[test]
    fn baseline_ranks_recent_and_reinforced_first() {
        let fresh = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: 0,
            seen: 1,
        });
        let old = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: 365,
            seen: 1,
        });
        assert!(fresh > old);
    }

    #[test]
    fn a_future_dated_fact_is_treated_as_today() {
        // Clock skew across synced devices must not produce a score that
        // grows without bound.
        let future = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: -30,
            seen: 1,
        });
        let today = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: 0,
            seen: 1,
        });
        assert_eq!(future, today);
    }

    #[test]
    fn seen_zero_is_treated_as_stated_once() {
        // A fact written before `seen::` existed parses as 0; it must not
        // score below one written today with seen = 1.
        let legacy = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: 0,
            seen: 0,
        });
        let once = baseline(&Candidate {
            text: "a",
            topics: &[],
            age_days: 0,
            seen: 1,
        });
        assert_eq!(legacy, once);
    }
}
