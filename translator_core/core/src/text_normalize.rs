//! 英文大小写恢复（recapitalization / recaser）。
//!
//! ## 为什么需要
//!
//! 双语 zipformer（cjkchar+bpe）模型对英文的输出**几乎总是全大写**
//! （"THREE" / "JACK WATCH"）—— 这是该模型训练数据与建模单元的
//! 已知特性（ sherpa 官方 demo 同样如此）。全大写上屏观感很差，
//! 用户明确反馈「大小写不对」。
//!
//! ## 方案：轻量启发式 recaser（不引第三方模型）
//!
//! 业界标准做法是小型 recaser 模型（BERT 级别），但对字幕场景过重。
//! 这里用「常见词表 + 句首大写 + 非常驻词按专有名词处理」：
//!
//! 1. 仅处理**全大写英文段**（含字母且字母全大写）—— 正常大小写文本原样返回
//! 2. 句首（文本开头或 `.`/`!`/`?` 后）首词大写
//! 3. 命中 [`COMMON_WORDS`] 常见词表的词 → 小写（the / is / three…）
//! 4. 未命中词表的词 → 视作专有名词，仅首字母大写（JACK→Jack, LUCY→Lucy）
//! 5. 单字母 "i" → "I"
//!
//! 效果：`THREE`→`Three`，`MY NAME IS JACK`→`My name is Jack`，
//! `JACK WATCH`→`Jack Watch`（JACK/WATCH 均不在表内，按专有名词）。
//!
//! 词表覆盖中小学 + 日常高频词（约 700 个），专有名词（人名/地名/
//! 品牌）自然落入「未命中→首字母大写」分支。

/// 常见英文词（小写形式）。命中 → 输出小写（句首除外）。
///
/// 收录原则：功能词（冠词/介词/连词/代词/助动词）+ 中小学高频实词。
/// **不收录**常见人名（jack/lucy/tom/lily…）—— 它们在字幕里多是
/// 专有名词，走「首字母大写」分支更合适。
const COMMON_WORDS: &[&str] = &[
    // 冠词 / 连词 / 介词
    "a", "an", "the", "and", "or", "but", "if", "because", "so", "as",
    "of", "to", "in", "on", "at", "for", "with", "by", "from", "about",
    "into", "over", "under", "after", "before", "between", "during",
    "without", "within", "against", "among", "across", "behind", "beside",
    "near", "until", "till", "since", "though", "although", "unless",
    "while", "whereas", "whether", "than", "via", "per", "upon", "toward",
    "towards", "inside", "outside", "along", "around", "round", "past",
    // 代词
    "i", "me", "my", "mine", "myself", "you", "your", "yours", "yourself",
    "he", "him", "his", "himself", "she", "her", "hers", "herself",
    "it", "its", "itself", "we", "us", "our", "ours", "ourselves",
    "they", "them", "their", "theirs", "themselves", "one", "ones",
    "this", "that", "these", "those", "such", "what", "which", "who",
    "whom", "whose", "when", "where", "why", "how", "whatever", "whoever",
    // be / 助动词 / 情态
    "am", "is", "are", "was", "were", "be", "been", "being", "do", "does",
    "did", "done", "doing", "have", "has", "had", "having", "will", "would",
    "shall", "should", "can", "could", "may", "might", "must", "ought",
    "need", "dare", "used", "let", "get", "got", "gotten",
    // 否定 / 程度 / 频率
    "no", "not", "none", "nor", "never", "nothing", "nobody", "nowhere",
    "yes", "yeah", "very", "too", "quite", "rather", "really", "much",
    "more", "most", "less", "least", "many", "few", "little", "lot",
    "lots", "enough", "almost", "nearly", "hardly", "barely", "just",
    "only", "even", "still", "yet", "already", "always", "usually",
    "often", "sometimes", "seldom", "rarely", "again", "once", "twice",
    "ever", "never", "soon", "later", "now", "then", "today", "tomorrow",
    "yesterday", "tonight", "morning", "afternoon", "evening", "night",
    "day", "week", "month", "year", "time", "times", "hour", "minute",
    "second", "moment", "early", "late", "first", "last", "next", "other",
    "another", "same", "different", "some", "any", "all", "both", "each",
    "every", "either", "neither", "several", "few", "many",
    // 常用动词
    "go", "goes", "going", "went", "gone", "come", "comes", "came",
    "make", "makes", "made", "take", "takes", "took", "taken", "see",
    "sees", "saw", "seen", "know", "knows", "knew", "known", "think",
    "thinks", "thought", "want", "wants", "wanted", "give", "gives",
    "gave", "given", "use", "uses", "find", "finds", "found", "tell",
    "tells", "told", "ask", "asks", "asked", "work", "works", "seem",
    "seems", "feel", "feels", "felt", "try", "tries", "tried", "leave",
    "leaves", "left", "call", "calls", "called", "keep", "keeps", "kept",
    "help", "helps", "talk", "talks", "turn", "turns", "start", "starts",
    "started", "show", "shows", "showed", "hear", "hears", "heard", "play",
    "plays", "played", "run", "runs", "ran", "move", "moves", "moved",
    "live", "lives", "lived", "believe", "bring", "brings", "brought",
    "happen", "happens", "write", "writes", "wrote", "written", "sit",
    "sits", "sat", "stand", "stands", "stood", "lose", "loses", "lost",
    "pay", "pays", "paid", "meet", "meets", "met", "include", "continue",
    "set", "learn", "learns", "learned", "change", "lead", "understand",
    "watch", "watches", "follow", "stop", "stops", "create", "speak",
    "speaks", "spoke", "read", "reads", "spend", "spent", "grow", "open",
    "opens", "walk", "walks", "win", "wins", "won", "offer", "remember",
    "love", "consider", "appear", "buy", "buys", "bought", "wait",
    "serve", "die", "send", "sends", "sent", "expect", "build", "stay",
    "fall", "cut", "reach", "remain", "suggest", "raise", "pass", "sell",
    "require", "report", "decide", "pull",
    // 常用名词
    "thing", "things", "way", "ways", "life", "world", "school", "home",
    "house", "room", "family", "father", "mother", "parent", "parents",
    "brother", "sister", "son", "daughter", "child", "children", "baby",
    "friend", "friends", "people", "person", "man", "men", "woman",
    "women", "boy", "girl", "boy", "teacher", "student", "students",
    "class", "classroom", "book", "books", "pen", "pencil", "paper",
    "desk", "chair", "table", "bed", "door", "window", "floor", "wall",
    "food", "water", "rice", "bread", "milk", "tea", "coffee", "juice",
    "egg", "eggs", "meat", "fish", "chicken", "fruit", "apple", "banana",
    "orange", "vegetable", "cake", "candy", "meal", "breakfast", "lunch",
    "dinner", "money", "price", "job", "work", "word", "words", "name",
    "names", "number", "numbers", "question", "answer", "problem",
    "story", "song", "music", "movie", "film", "game", "sport", "ball",
    "team", "car", "bus", "bike", "train", "plane", "ship", "road",
    "street", "city", "town", "village", "country", "place", "park",
    "shop", "store", "hospital", "library", "office", "farm", "zoo",
    "animal", "animals", "dog", "cat", "bird", "horse", "cow", "pig",
    "sheep", "monkey", "tiger", "lion", "bear", "rabbit", "duck", "tree",
    "trees", "flower", "grass", "sun", "moon", "star", "sky", "cloud",
    "rain", "snow", "wind", "weather", "season", "spring", "summer",
    "autumn", "fall", "winter", "body", "head", "hair", "face", "eye",
    "eyes", "ear", "nose", "mouth", "hand", "hands", "foot", "feet",
    "arm", "leg", "heart", "voice", "sound", "color", "red", "blue",
    "green", "yellow", "black", "white", "brown", "orange", "pink",
    "grey", "light", "dark", "big", "small", "long", "short", "tall",
    "high", "low", "new", "old", "young", "good", "bad", "nice", "fine",
    "great", "happy", "sad", "angry", "tired", "hungry", "full", "free",
    "busy", "ready", "sure", "clean", "dirty", "easy", "hard", "difficult",
    "important", "beautiful", "kind", "warm", "cool", "cold", "hot",
    "fast", "slow", "strong", "weak", "right", "wrong", "true", "real",
    "best", "better", "worse", "worst", "happy", "sorry", "hello", "hi",
    "goodbye", "bye", "please", "thanks", "welcome", "okay", "ok",
    // 数词 / 量词
    "zero", "one", "two", "three", "four", "five", "six", "seven",
    "eight", "nine", "ten", "eleven", "twelve", "twenty", "thirty",
    "hundred", "thousand", "million", "half", "quarter", "pair",
    "piece", "piece", "box", "bag", "cup", "glass", "bottle", "plate",
    // 学科 / 文具 / 其他高频
    "english", "chinese", "math", "maths", "science", "history", "music",
    "art", "pe", "computer", "phone", "radio", "tv", "picture", "photo",
    "letter", "card", "gift", "party", "trip", "holiday", "vacation",
    "test", "exam", "lesson", "homework", "subject", "grade", "mark",
    "score", "rule", "example", "exercise", "practice", "speech", "talk",
    "meeting", "plan", "idea", "news", "information", "message", "email",
    "street", "map", "ticket", "key", "box", "rule", "law", "right",
    "health", "doctor", "nurse", "police", "king", "queen",
];

/// 是否全大写英文段（>=2 个字母且字母全大写、无小写）。
fn is_all_caps_english(text: &str) -> bool {
    let mut upper = 0usize;
    let mut lower = 0usize;
    for c in text.chars() {
        if c.is_ascii_uppercase() {
            upper += 1;
        } else if c.is_ascii_lowercase() {
            lower += 1;
        }
    }
    upper >= 2 && lower == 0
}

/// 词首字母大写（其余不动）。
fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// 英文大小写恢复。输入非全大写英文则原样返回。
///
/// 规则见模块文档。标点后的第一个词也视为句首。
pub fn recapitalize_english(text: &str) -> String {
    if !is_all_caps_english(text) {
        return text.to_string();
    }
    // CJK 内容（双语模型可能吐中文+英文混合）不处理大小写，
    // 但段内英文词仍逐个过规则 —— 按空白切分即可，CJK 词不在表内
    // 会被 capitalize_first 无意义地大写首字母？CJK 无大小写，
    // to_ascii_uppercase 对非 ASCII 是恒等，安全。
    let mut out = String::with_capacity(text.len());
    let mut sentence_start = true;
    for tok in text.split_inclusive(' ') {
        // 分离词尾标点空白：split_inclusive 已保留空白
        let trimmed = tok.trim_end_matches([' ']);
        let has_space = tok.len() > trimmed.len();
        let word = recap_one_word(trimmed, sentence_start);
        out.push_str(&word);
        if has_space {
            out.push(' ');
        }
        // 句末标点 → 下一词为句首
        sentence_start = trimmed.ends_with(['.', '!', '?', ':', '。', '！', '？'])
            || sentence_start && trimmed.is_empty();
    }
    out
}

/// 处理单个词（可能带首尾标点，如 `"HELLO,"` / `"JACK."`）。
fn recap_one_word(tok: &str, sentence_start: bool) -> String {
    if tok.is_empty() {
        return String::new();
    }
    // 拆出核心词（跳过首尾非字母字符）
    let start = tok.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(0);
    let end = tok
        .rfind(|c: char| c.is_ascii_alphabetic())
        .map(|i| i + 1)
        .unwrap_or(tok.len());
    if start >= end {
        return tok.to_string(); // 无字母（纯标点/数字/CJK）原样
    }
    let core = &tok[start..end];
    let lower = core.to_ascii_lowercase();
    let fixed = if lower == "i" {
        "I".to_string()
    } else if COMMON_WORDS.contains(&lower.as_str()) {
        if sentence_start {
            capitalize_first(&lower)
        } else {
            lower.clone()
        }
    } else {
        // 未命中 → 专有名词：整词转小写后首字母大写
        // （JACK→Jack, LUCY→Lucy。注意不能只大写首字母 ——
        // 原词其余部分是全大写，会得到 "JACK"→"JACK"）
        capitalize_first(&lower)
    };
    format!("{}{}{}", &tok[..start], fixed, &tok[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_caps_basic() {
        assert_eq!(recapitalize_english("THREE"), "Three");
        assert_eq!(recapitalize_english("MY NAME IS JACK"), "My name is Jack");
        assert_eq!(recapitalize_english("I HAVE A DOG"), "I have a dog");
    }

    #[test]
    fn proper_nouns_keep_cap() {
        // LUCY 不在常见词表 → 专有名词分支 → Lucy。
        assert_eq!(recapitalize_english("HELLO, LUCY"), "Hello, Lucy");
        // JACK WATCH：JACK 不在表 → Jack；watch 是常见词（I WATCH TV）→ 小写。
        // 这是「常见词优先小写」的已知权衡 —— 听力教材里 "Jack Watch"
        // 会显示成 "Jack watch"，可接受（比全大写好得多）。
        assert_eq!(recapitalize_english("JACK WATCH"), "Jack watch");
    }

    #[test]
    fn sentence_start_after_period() {
        assert_eq!(
            recapitalize_english("GOOD MORNING. HOW ARE YOU"),
            "Good morning. How are you"
        );
    }

    #[test]
    fn i_always_upper() {
        assert_eq!(recapitalize_english("YOU AND I"), "You and I");
    }

    #[test]
    fn non_caps_passthrough() {
        assert_eq!(recapitalize_english("Hello World"), "Hello World");
        assert_eq!(recapitalize_english("这是中文"), "这是中文");
    }

    #[test]
    fn mixed_with_punct() {
        assert_eq!(recapitalize_english("THREE, PLEASE."), "Three, please.");
    }
}
