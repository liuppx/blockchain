//! Cryptographic identity for the node: ed25519 signatures over transactions.
//!
//! Thin wrappers around the audited `ed25519-dalek` crate. We deliberately do
//! NOT implement the signature scheme ourselves. Keys here are raw 32-byte
//! public keys and 64-byte signatures so the rest of the node (codec, state,
//! hashing) stays plain bytes.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub type PubKey = [u8; 32];
pub type Sig = [u8; 64];

/// A signing keypair. `from_seed` is deterministic (no RNG), which keeps demos
/// and tests reproducible; production keys come from a CSPRNG / HSM.
///
/// `Clone` (M40): the daemon hands a signing clone to the authenticated-handshake
/// context while the consensus actor keeps its own owned copy — a `SigningKey` is
/// itself `Clone`, so this is a cheap, sound copy of the secret scalar.
///
/// `Debug` (M169): the panic messages from `.unwrap()` on the config loader
/// print the inner `Keypair`; deriving `Debug` (just a derived marker — no
/// secret material is exposed because `ed25519_dalek::SigningKey` formats
/// only the public part) keeps the test diagnostics readable without
/// exposing the seed.
#[derive(Clone, Debug)]
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Keypair(SigningKey::from_bytes(&seed))
    }

    pub fn public(&self) -> PubKey {
        self.0.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> Sig {
        self.0.sign(msg).to_bytes()
    }

    /// The raw 32-byte ed25519 seed, round-tripping with `from_seed`.
    ///
    /// Used (M43) to derive a PKCS#8 encoding for the genesis-pinned mTLS
    /// credential, so a node's TLS identity *is* its consensus key. Kept crate-
    /// internal in spirit — it exposes secret material, so only the daemon's
    /// TLS setup calls it.
    pub fn secret_seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

/// Verify `sig` over `msg` against `pk`. Returns false on any malformed input
/// (bad point encoding, wrong signature) rather than erroring.
pub fn verify(pk: &PubKey, msg: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pk) else {
        return false;
    };
    let signature = Signature::from_bytes(sig);
    vk.verify(msg, &signature).is_ok()
}

// ---- M169: BIP-39 mnemonic keys (backup / recovery for the 32-byte seed) -----
//
// BIP-39 (https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)
// specifies a 12/15/18/21/24-word mnemonic derived from 128/160/192/224/256 bits
// of entropy, plus an optional passphrase ("25th word"). The mnemonic is
// human-readable backup; `mnemonic_to_seed` deterministically derives the
// 32-byte ed25519 seed that `Keypair::from_seed` consumes, so the rest of the
// node (consensus, mTLS, encode-tx) is unaffected — only the keygen input
// path changes.
//
// M169 simplifications (documented limits; M170 may lift them):
//   * ASCII-only input. Non-ASCII mnemonics / passphrases return
//     `MnemonicError::NonAscii`. The BIP-39 spec requires NFKD normalization;
//     we skip it because every word in the 2048-word English list is ASCII
//     and most operator passphrases are too. Document, do not silently
//     mangle.
//   * Wordlist is inlined (~25 KB) rather than pulling the `bip39` crate, to
//     keep M169's dep surface flat (sha2 is the only new code path).
//   * PBKDF2-HMAC-SHA512 is hand-rolled (~30 lines) rather than via the
//     `pbkdf2`/`pkcs5` crate, for the same reason.

use sha2::{Digest, Sha256};

/// One-shot SHA-256 → `[u8; 32]`. Mirrors the M121 `hash::sha256` shape but
/// kept crate-local to avoid leaking `sha2` into the rest of the codebase.
fn sha256_one(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// Lowercase hex encoding. Local mirror of `crate::hash::hex` so the BIP-39
/// tests don't have to thread an extra import through `mod tests`.
/// `#[allow(dead_code)]` (M168) — the function is only referenced from
/// unit tests in this file, but the compiler doesn't see those references
/// during lib builds.
#[allow(dead_code)]
fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// All errors `mnemonic_to_seed` / `validate_mnemonic` / `mnemonic_generate`
/// can surface. Display impl renders a single-line message safe for CLI
/// diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MnemonicError {
    /// Word count is not in {12, 15, 18, 21, 24}.
    InvalidLength(usize),
    /// A non-ASCII byte was seen in the mnemonic or passphrase.
    NonAscii,
    /// A word in the mnemonic is not in the BIP-39 English list.
    UnknownWord(String),
    /// The trailing-word checksum does not match the entropy hash (BIP-39 §"Generating the mnemonic").
    BadChecksum,
}

impl std::fmt::Display for MnemonicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MnemonicError::InvalidLength(n) => {
                write!(f, "mnemonic has {n} words; BIP-39 requires 12/15/18/21/24")
            }
            MnemonicError::NonAscii => f.write_str(
                "mnemonic/passphrase must be ASCII (BIP-39 NFKD support is a follow-up)",
            ),
            MnemonicError::UnknownWord(w) => write!(f, "unknown BIP-39 word: {w:?}"),
            MnemonicError::BadChecksum => {
                f.write_str("BIP-39 checksum mismatch (typo or wrong word)")
            }
        }
    }
}

impl std::error::Error for MnemonicError {}

/// Look up a BIP-39 English word's index. Returns `None` if the word is not
/// in the 2048-entry list. The list is stored in a `match`-equivalent binary
/// search to keep this O(log n) without pulling a `bip39` crate.
fn word_index(word: &str) -> Option<u16> {
    // Linear scan over 2048 words; the build is one-shot and the function
    // is only called during keygen (offline) so the simple approach is
    // fine and keeps the binary tiny. If this ever shows up in a hot
    // path, swap for `bip39::Language::English.find_word(word)`.
    BIP39_WORDLIST
        .iter()
        .position(|w| *w == word)
        .map(|i| i as u16)
}

/// Look up a BIP-39 English word by index. Panics on out-of-range — callers
/// pass indices derived from the entropy, so an OOB index indicates a bug
/// in the entropy-bit-grouping math below.
fn word_at(idx: u16) -> &'static str {
    BIP39_WORDLIST
        .get(idx as usize)
        .expect("BIP-39 index out of range")
}

/// BIP-39 §"From mnemonic to seed": `PBKDF2-HMAC-SHA512(mnemonic, "mnemonic"+passphrase, 2048)`,
/// producing a 64-byte seed. ASCII-only — non-ASCII input returns
/// `Err(MnemonicError::NonAscii)`. The 32-byte ed25519 seed consumed by
/// [`Keypair::from_seed`] is the **first 32 bytes** of this 64-byte output
/// (BIP-39 leaves the other 32 bytes for the chain code in HD wallets, which
/// we don't need; the same convention is used by Trezor / SLIP-0010).
pub fn mnemonic_to_seed(
    words: &str,
    passphrase: &str,
    out: &mut [u8; 32],
) -> Result<(), MnemonicError> {
    if !words.is_ascii() || !passphrase.is_ascii() {
        return Err(MnemonicError::NonAscii);
    }
    // BIP-39 spec §"From mnemonic to seed": salt is `"mnemonic"` concatenated
    // with the passphrase. The `"mnemonic"` prefix is the only difference
    // between the empty-passphrase case and just hashing the words.
    let salt = format!("mnemonic{}", passphrase);
    let mut full = [0u8; 64];
    pbkdf2_hmac_sha512(words.as_bytes(), salt.as_bytes(), 2048, &mut full);
    out.copy_from_slice(&full[..32]);
    Ok(())
}

/// Verify a BIP-39 mnemonic: word count, word membership, trailing-word
/// checksum. Does NOT normalize (M169 is ASCII-only — see top-of-module).
pub fn validate_mnemonic(words: &str) -> Result<(), MnemonicError> {
    if !words.is_ascii() {
        return Err(MnemonicError::NonAscii);
    }
    let parts: Vec<&str> = words.split_whitespace().collect();
    let expected_ms = match parts.len() {
        12 => 12,
        15 => 15,
        18 => 18,
        21 => 21,
        24 => 24,
        n => return Err(MnemonicError::InvalidLength(n)),
    };
    let mut bits: Vec<u8> = Vec::with_capacity(expected_ms * 11 / 8 + 1);
    for w in &parts {
        let idx = word_index(w).ok_or_else(|| MnemonicError::UnknownWord((*w).to_string()))?;
        // 11 bits per word, MSB-first within the word (matching the
        // generation side): push bit 10 down to bit 0.
        for b in (0u16..=10).rev() {
            bits.push(((idx >> b) & 1) as u8);
        }
    }
    // Split bits into entropy (ms*33/32 bytes worth of bits) and checksum
    // (1 byte for ms=12..18, 2 bytes for ms=21, 3 bytes for ms=24 — but
    // BIP-39 spec uses 4/5/6/7/8 bits of SHA256 prefix respectively).
    let checksum_bits = expected_ms / 3; // 4, 5, 6, 7, 8
    let entropy_bits = expected_ms * 11 - checksum_bits;
    let mut entropy = vec![0u8; entropy_bits / 8];
    for (i, b) in entropy.iter_mut().enumerate() {
        let mut byte = 0u8;
        for j in 0..8 {
            byte = (byte << 1) | bits[i * 8 + j];
        }
        *b = byte;
    }
    // Extract the trailing checksum bits.
    let mut checksum = 0u8;
    for j in 0..checksum_bits {
        checksum = (checksum << 1) | bits[entropy_bits + j];
    }
    let expected = sha256_one(&entropy);
    let actual = &expected[0..(checksum_bits / 8 + if checksum_bits % 8 == 0 { 0 } else { 1 })];
    // For 12/15/18: 4/5/6 bits of SHA256 first byte. For 21: 7 bits of first byte.
    // For 24: 8 bits = full first byte. Compare the masked prefix.
    let valid = match checksum_bits {
        4..=7 => checksum == (actual[0] >> (8 - checksum_bits)),
        8 => checksum == actual[0],
        _ => unreachable!(),
    };
    if valid {
        Ok(())
    } else {
        Err(MnemonicError::BadChecksum)
    }
}

/// Derive a `ms` (12/15/18/21/24) word mnemonic from `entropy` (must be the
/// matching byte length: 16/20/24/28/32). `ms` and `entropy.len()*8 + ms/3`
/// must equal 132/165/198/231/264 respectively. Returns the words joined by
/// single spaces.
pub fn mnemonic_generate(entropy: &[u8]) -> Result<String, MnemonicError> {
    let expected = match entropy.len() {
        16 => 12,
        20 => 15,
        24 => 18,
        28 => 21,
        32 => 24,
        n => return Err(MnemonicError::InvalidLength(n)),
    };
    let checksum_bits = expected / 3;
    // Hash the entropy to get the checksum.
    let hash = sha256_one(entropy);
    // Concatenate entropy bits + checksum prefix bits, then group 11 at a time.
    // BIP-39 §"Generating the mnemonic": the first bit of the entropy is the
    // MSB of entropy[0], so we push bits MSB-first (bit 7 → bit 0).
    let total_bits = entropy.len() * 8 + checksum_bits;
    let mut bits: Vec<u8> = Vec::with_capacity(total_bits);
    for b in entropy {
        for j in (0..8).rev() {
            bits.push((b >> j) & 1);
        }
    }
    for j in 0..checksum_bits {
        bits.push((hash[j / 8] >> (7 - (j % 8))) & 1);
    }
    let mut words = Vec::with_capacity(expected);
    for i in 0..expected {
        let mut idx: u16 = 0;
        for j in 0..11 {
            idx = (idx << 1) | (bits[i * 11 + j] as u16);
        }
        words.push(word_at(idx).to_string());
    }
    Ok(words.join(" "))
}

// ---- BIP-39 wordlist (2048 English words, public standard) ---------------

static BIP39_WORDLIST: [&str; 2048] = [
    "abandon", "ability", "able", "about", "above", "absent", "absorb", "abstract", "absurd",
    "abuse", "access", "accident", "account", "accuse", "achieve", "acid", "acoustic", "acquire",
    "across", "act", "action", "actor", "actress", "actual", "adapt", "add", "addict", "address",
    "adjust", "admit", "adult", "advance", "advice", "aerobic", "affair", "afford", "afraid",
    "again", "age", "agent", "agree", "ahead", "aim", "air", "airport", "aisle", "alarm", "album",
    "alcohol", "alert", "alien", "all", "alley", "allow", "almost", "alone", "alpha", "already",
    "also", "alter", "always", "amateur", "amazing", "among", "amount", "amused", "analyst",
    "anchor", "ancient", "anger", "angle", "angry", "animal", "ankle", "announce", "annual",
    "another", "answer", "antenna", "antique", "anxiety", "any", "apart", "apology", "appear",
    "apple", "approve", "april", "arch", "arctic", "area", "arena", "argue", "arm", "armed",
    "armor", "army", "around", "arrange", "arrest", "arrive", "arrow", "art", "artefact", "artist",
    "artwork", "ask", "aspect", "assault", "asset", "assist", "assume", "asthma", "athlete",
    "atom", "attack", "attend", "attitude", "attract", "auction", "audit", "august", "aunt",
    "author", "auto", "autumn", "average", "avocado", "avoid", "awake", "aware", "away", "awesome",
    "awful", "awkward", "axis", "baby", "bachelor", "bacon", "badge", "bag", "balance", "balcony",
    "ball", "bamboo", "banana", "banner", "bar", "barely", "bargain", "barrel", "base", "basic",
    "basket", "battle", "beach", "bean", "beauty", "because", "become", "beef", "before", "begin",
    "behave", "behind", "believe", "below", "belt", "bench", "benefit", "best", "betray", "better",
    "between", "beyond", "bicycle", "bid", "bike", "bind", "biology", "bird", "birth", "bitter",
    "black", "blade", "blame", "blanket", "blast", "bleak", "bless", "blind", "blood", "blossom",
    "blouse", "blue", "blur", "blush", "board", "boat", "body", "boil", "bomb", "bone", "bonus",
    "book", "boost", "border", "boring", "borrow", "boss", "bottom", "bounce", "box", "boy",
    "bracket", "brain", "brand", "brass", "brave", "bread", "breeze", "brick", "bridge", "brief",
    "bright", "bring", "brisk", "broccoli", "broken", "bronze", "broom", "brother", "brown",
    "brush", "bubble", "buddy", "budget", "buffalo", "build", "bulb", "bulk", "bullet", "bundle",
    "bunker", "burden", "burger", "burst", "bus", "business", "busy", "butter", "buyer", "buzz",
    "cabbage", "cabin", "cable", "cactus", "cage", "cake", "call", "calm", "camera", "camp", "can",
    "canal", "cancel", "candy", "cannon", "canoe", "canvas", "canyon", "capable", "capital",
    "captain", "car", "carbon", "card", "cargo", "carpet", "carry", "cart", "case", "cash",
    "casino", "castle", "casual", "cat", "catalog", "catch", "category", "cattle", "caught",
    "cause", "caution", "cave", "ceiling", "celery", "cement", "census", "century", "cereal",
    "certain", "chair", "chalk", "champion", "change", "chaos", "chapter", "charge", "chase",
    "chat", "cheap", "check", "cheese", "chef", "cherry", "chest", "chicken", "chief", "child",
    "chimney", "choice", "choose", "chronic", "chuckle", "chunk", "churn", "cigar", "cinnamon",
    "circle", "citizen", "city", "civil", "claim", "clap", "clarify", "claw", "clay", "clean",
    "clerk", "clever", "click", "client", "cliff", "climb", "clinic", "clip", "clock", "clog",
    "close", "cloth", "cloud", "clown", "club", "clump", "cluster", "clutch", "coach", "coast",
    "coconut", "code", "coffee", "coil", "coin", "collect", "color", "column", "combine", "come",
    "comfort", "comic", "common", "company", "concert", "conduct", "confirm", "congress",
    "connect", "consider", "control", "convince", "cook", "cool", "copper", "copy", "coral",
    "core", "corn", "correct", "cost", "cotton", "couch", "country", "couple", "course", "cousin",
    "cover", "coyote", "crack", "cradle", "craft", "cram", "crane", "crash", "crater", "crawl",
    "crazy", "cream", "credit", "creek", "crew", "cricket", "crime", "crisp", "critic", "crop",
    "cross", "crouch", "crowd", "crucial", "cruel", "cruise", "crumble", "crunch", "crush", "cry",
    "crystal", "cube", "culture", "cup", "cupboard", "curious", "current", "curtain", "curve",
    "cushion", "custom", "cute", "cycle", "dad", "damage", "damp", "dance", "danger", "daring",
    "dash", "daughter", "dawn", "day", "deal", "debate", "debris", "decade", "december", "decide",
    "decline", "decorate", "decrease", "deer", "defense", "define", "defy", "degree", "delay",
    "deliver", "demand", "demise", "denial", "dentist", "deny", "depart", "depend", "deposit",
    "depth", "deputy", "derive", "describe", "desert", "design", "desk", "despair", "destroy",
    "detail", "detect", "develop", "device", "devote", "diagram", "dial", "diamond", "diary",
    "dice", "diesel", "diet", "differ", "digital", "dignity", "dilemma", "dinner", "dinosaur",
    "direct", "dirt", "disagree", "discover", "disease", "dish", "dismiss", "disorder", "display",
    "distance", "divert", "divide", "divorce", "dizzy", "doctor", "document", "dog", "doll",
    "dolphin", "domain", "donate", "donkey", "donor", "door", "dose", "double", "dove", "draft",
    "dragon", "drama", "drastic", "draw", "dream", "dress", "drift", "drill", "drink", "drip",
    "drive", "drop", "drum", "dry", "duck", "dumb", "dune", "during", "dust", "dutch", "duty",
    "dwarf", "dynamic", "eager", "eagle", "early", "earn", "earth", "easily", "east", "easy",
    "echo", "ecology", "economy", "edge", "edit", "educate", "effort", "egg", "eight", "either",
    "elbow", "elder", "electric", "elegant", "element", "elephant", "elevator", "elite", "else",
    "embark", "embody", "embrace", "emerge", "emotion", "employ", "empower", "empty", "enable",
    "enact", "end", "endless", "endorse", "enemy", "energy", "enforce", "engage", "engine",
    "enhance", "enjoy", "enlist", "enough", "enrich", "enroll", "ensure", "enter", "entire",
    "entry", "envelope", "episode", "equal", "equip", "era", "erase", "erode", "erosion", "error",
    "erupt", "escape", "essay", "essence", "estate", "eternal", "ethics", "evidence", "evil",
    "evoke", "evolve", "exact", "example", "excess", "exchange", "excite", "exclude", "excuse",
    "execute", "exercise", "exhaust", "exhibit", "exile", "exist", "exit", "exotic", "expand",
    "expect", "expire", "explain", "expose", "express", "extend", "extra", "eye", "eyebrow",
    "fabric", "face", "faculty", "fade", "faint", "faith", "fall", "false", "fame", "family",
    "famous", "fan", "fancy", "fantasy", "farm", "fashion", "fat", "fatal", "father", "fatigue",
    "fault", "favorite", "feature", "february", "federal", "fee", "feed", "feel", "female",
    "fence", "festival", "fetch", "fever", "few", "fiber", "fiction", "field", "figure", "file",
    "film", "filter", "final", "find", "fine", "finger", "finish", "fire", "firm", "first",
    "fiscal", "fish", "fit", "fitness", "fix", "flag", "flame", "flash", "flat", "flavor", "flee",
    "flight", "flip", "float", "flock", "floor", "flower", "fluid", "flush", "fly", "foam",
    "focus", "fog", "foil", "fold", "follow", "food", "foot", "force", "forest", "forget", "fork",
    "fortune", "forum", "forward", "fossil", "foster", "found", "fox", "fragile", "frame",
    "frequent", "fresh", "friend", "fringe", "frog", "front", "frost", "frown", "frozen", "fruit",
    "fuel", "fun", "funny", "furnace", "fury", "future", "gadget", "gain", "galaxy", "gallery",
    "game", "gap", "garage", "garbage", "garden", "garlic", "garment", "gas", "gasp", "gate",
    "gather", "gauge", "gaze", "general", "genius", "genre", "gentle", "genuine", "gesture",
    "ghost", "giant", "gift", "giggle", "ginger", "giraffe", "girl", "give", "glad", "glance",
    "glare", "glass", "glide", "glimpse", "globe", "gloom", "glory", "glove", "glow", "glue",
    "goat", "goddess", "gold", "good", "goose", "gorilla", "gospel", "gossip", "govern", "gown",
    "grab", "grace", "grain", "grant", "grape", "grass", "gravity", "great", "green", "grid",
    "grief", "grit", "grocery", "group", "grow", "grunt", "guard", "guess", "guide", "guilt",
    "guitar", "gun", "gym", "habit", "hair", "half", "hammer", "hamster", "hand", "happy",
    "harbor", "hard", "harsh", "harvest", "hat", "have", "hawk", "hazard", "head", "health",
    "heart", "heavy", "hedgehog", "height", "hello", "helmet", "help", "hen", "hero", "hidden",
    "high", "hill", "hint", "hip", "hire", "history", "hobby", "hockey", "hold", "hole", "holiday",
    "hollow", "home", "honey", "hood", "hope", "horn", "horror", "horse", "hospital", "host",
    "hotel", "hour", "hover", "hub", "huge", "human", "humble", "humor", "hundred", "hungry",
    "hunt", "hurdle", "hurry", "hurt", "husband", "hybrid", "ice", "icon", "idea", "identify",
    "idle", "ignore", "ill", "illegal", "illness", "image", "imitate", "immense", "immune",
    "impact", "impose", "improve", "impulse", "inch", "include", "income", "increase", "index",
    "indicate", "indoor", "industry", "infant", "inflict", "inform", "inhale", "inherit",
    "initial", "inject", "injury", "inmate", "inner", "innocent", "input", "inquiry", "insane",
    "insect", "inside", "inspire", "install", "intact", "interest", "into", "invest", "invite",
    "involve", "iron", "island", "isolate", "issue", "item", "ivory", "jacket", "jaguar", "jar",
    "jazz", "jealous", "jeans", "jelly", "jewel", "job", "join", "joke", "journey", "joy", "judge",
    "juice", "jump", "jungle", "junior", "junk", "just", "kangaroo", "keen", "keep", "ketchup",
    "key", "kick", "kid", "kidney", "kind", "kingdom", "kiss", "kit", "kitchen", "kite", "kitten",
    "kiwi", "knee", "knife", "knock", "know", "lab", "label", "labor", "ladder", "lady", "lake",
    "lamp", "language", "laptop", "large", "later", "latin", "laugh", "laundry", "lava", "law",
    "lawn", "lawsuit", "layer", "lazy", "leader", "leaf", "learn", "leave", "lecture", "left",
    "leg", "legal", "legend", "leisure", "lemon", "lend", "length", "lens", "leopard", "lesson",
    "letter", "level", "liar", "liberty", "library", "license", "life", "lift", "light", "like",
    "limb", "limit", "link", "lion", "liquid", "list", "little", "live", "lizard", "load", "loan",
    "lobster", "local", "lock", "logic", "lonely", "long", "loop", "lottery", "loud", "lounge",
    "love", "loyal", "lucky", "luggage", "lumber", "lunar", "lunch", "luxury", "lyrics", "machine",
    "mad", "magic", "magnet", "maid", "mail", "main", "major", "make", "mammal", "man", "manage",
    "mandate", "mango", "mansion", "manual", "maple", "marble", "march", "margin", "marine",
    "market", "marriage", "mask", "mass", "master", "match", "material", "math", "matrix",
    "matter", "maximum", "maze", "meadow", "mean", "measure", "meat", "mechanic", "medal", "media",
    "melody", "melt", "member", "memory", "mention", "menu", "mercy", "merge", "merit", "merry",
    "mesh", "message", "metal", "method", "middle", "midnight", "milk", "million", "mimic", "mind",
    "minimum", "minor", "minute", "miracle", "mirror", "misery", "miss", "mistake", "mix", "mixed",
    "mixture", "mobile", "model", "modify", "mom", "moment", "monitor", "monkey", "monster",
    "month", "moon", "moral", "more", "morning", "mosquito", "mother", "motion", "motor",
    "mountain", "mouse", "move", "movie", "much", "muffin", "mule", "multiply", "muscle", "museum",
    "mushroom", "music", "must", "mutual", "myself", "mystery", "myth", "naive", "name", "napkin",
    "narrow", "nasty", "nation", "nature", "near", "neck", "need", "negative", "neglect",
    "neither", "nephew", "nerve", "nest", "net", "network", "neutral", "never", "news", "next",
    "nice", "night", "noble", "noise", "nominee", "noodle", "normal", "north", "nose", "notable",
    "note", "nothing", "notice", "novel", "now", "nuclear", "number", "nurse", "nut", "oak",
    "obey", "object", "oblige", "obscure", "observe", "obtain", "obvious", "occur", "ocean",
    "october", "odor", "off", "offer", "office", "often", "oil", "okay", "old", "olive", "olympic",
    "omit", "once", "one", "onion", "online", "only", "open", "opera", "opinion", "oppose",
    "option", "orange", "orbit", "orchard", "order", "ordinary", "organ", "orient", "original",
    "orphan", "ostrich", "other", "outdoor", "outer", "output", "outside", "oval", "oven", "over",
    "own", "owner", "oxygen", "oyster", "ozone", "pact", "paddle", "page", "pair", "palace",
    "palm", "panda", "panel", "panic", "panther", "paper", "parade", "parent", "park", "parrot",
    "party", "pass", "patch", "path", "patient", "patrol", "pattern", "pause", "pave", "payment",
    "peace", "peanut", "pear", "peasant", "pelican", "pen", "penalty", "pencil", "people",
    "pepper", "perfect", "permit", "person", "pet", "phone", "photo", "phrase", "physical",
    "piano", "picnic", "picture", "piece", "pig", "pigeon", "pill", "pilot", "pink", "pioneer",
    "pipe", "pistol", "pitch", "pizza", "place", "planet", "plastic", "plate", "play", "please",
    "pledge", "pluck", "plug", "plunge", "poem", "poet", "point", "polar", "pole", "police",
    "pond", "pony", "pool", "popular", "portion", "position", "possible", "post", "potato",
    "pottery", "poverty", "powder", "power", "practice", "praise", "predict", "prefer", "prepare",
    "present", "pretty", "prevent", "price", "pride", "primary", "print", "priority", "prison",
    "private", "prize", "problem", "process", "produce", "profit", "program", "project", "promote",
    "proof", "property", "prosper", "protect", "proud", "provide", "public", "pudding", "pull",
    "pulp", "pulse", "pumpkin", "punch", "pupil", "puppy", "purchase", "purity", "purpose",
    "purse", "push", "put", "puzzle", "pyramid", "quality", "quantum", "quarter", "question",
    "quick", "quit", "quiz", "quote", "rabbit", "raccoon", "race", "rack", "radar", "radio",
    "rail", "rain", "raise", "rally", "ramp", "ranch", "random", "range", "rapid", "rare", "rate",
    "rather", "raven", "raw", "razor", "ready", "real", "reason", "rebel", "rebuild", "recall",
    "receive", "recipe", "record", "recycle", "reduce", "reflect", "reform", "refuse", "region",
    "regret", "regular", "reject", "relax", "release", "relief", "rely", "remain", "remember",
    "remind", "remove", "render", "renew", "rent", "reopen", "repair", "repeat", "replace",
    "report", "require", "rescue", "resemble", "resist", "resource", "response", "result",
    "retire", "retreat", "return", "reunion", "reveal", "review", "reward", "rhythm", "rib",
    "ribbon", "rice", "rich", "ride", "ridge", "rifle", "right", "rigid", "ring", "riot", "ripple",
    "risk", "ritual", "rival", "river", "road", "roast", "robot", "robust", "rocket", "romance",
    "roof", "rookie", "room", "rose", "rotate", "rough", "round", "route", "royal", "rubber",
    "rude", "rug", "rule", "run", "runway", "rural", "sad", "saddle", "sadness", "safe", "sail",
    "salad", "salmon", "salon", "salt", "salute", "same", "sample", "sand", "satisfy", "satoshi",
    "sauce", "sausage", "save", "say", "scale", "scan", "scare", "scatter", "scene", "scheme",
    "school", "science", "scissors", "scorpion", "scout", "scrap", "screen", "script", "scrub",
    "sea", "search", "season", "seat", "second", "secret", "section", "security", "seed", "seek",
    "segment", "select", "sell", "seminar", "senior", "sense", "sentence", "series", "service",
    "session", "settle", "setup", "seven", "shadow", "shaft", "shallow", "share", "shed", "shell",
    "sheriff", "shield", "shift", "shine", "ship", "shiver", "shock", "shoe", "shoot", "shop",
    "short", "shoulder", "shove", "shrimp", "shrug", "shuffle", "shy", "sibling", "sick", "side",
    "siege", "sight", "sign", "silent", "silk", "silly", "silver", "similar", "simple", "since",
    "sing", "siren", "sister", "situate", "six", "size", "skate", "sketch", "ski", "skill", "skin",
    "skirt", "skull", "slab", "slam", "sleep", "slender", "slice", "slide", "slight", "slim",
    "slogan", "slot", "slow", "slush", "small", "smart", "smile", "smoke", "smooth", "snack",
    "snake", "snap", "sniff", "snow", "soap", "soccer", "social", "sock", "soda", "soft", "solar",
    "soldier", "solid", "solution", "solve", "someone", "song", "soon", "sorry", "sort", "soul",
    "sound", "soup", "source", "south", "space", "spare", "spatial", "spawn", "speak", "special",
    "speed", "spell", "spend", "sphere", "spice", "spider", "spike", "spin", "spirit", "split",
    "spoil", "sponsor", "spoon", "sport", "spot", "spray", "spread", "spring", "spy", "square",
    "squeeze", "squirrel", "stable", "stadium", "staff", "stage", "stairs", "stamp", "stand",
    "start", "state", "stay", "steak", "steel", "stem", "step", "stereo", "stick", "still",
    "sting", "stock", "stomach", "stone", "stool", "story", "stove", "strategy", "street",
    "strike", "strong", "struggle", "student", "stuff", "stumble", "style", "subject", "submit",
    "subway", "success", "such", "sudden", "suffer", "sugar", "suggest", "suit", "summer", "sun",
    "sunny", "sunset", "super", "supply", "supreme", "sure", "surface", "surge", "surprise",
    "surround", "survey", "suspect", "sustain", "swallow", "swamp", "swap", "swarm", "swear",
    "sweet", "swift", "swim", "swing", "switch", "sword", "symbol", "symptom", "syrup", "system",
    "table", "tackle", "tag", "tail", "talent", "talk", "tank", "tape", "target", "task", "taste",
    "tattoo", "taxi", "teach", "team", "tell", "ten", "tenant", "tennis", "tent", "term", "test",
    "text", "thank", "that", "theme", "then", "theory", "there", "they", "thing", "this",
    "thought", "three", "thrive", "throw", "thumb", "thunder", "ticket", "tide", "tiger", "tilt",
    "timber", "time", "tiny", "tip", "tired", "tissue", "title", "toast", "tobacco", "today",
    "toddler", "toe", "together", "toilet", "token", "tomato", "tomorrow", "tone", "tongue",
    "tonight", "tool", "tooth", "top", "topic", "topple", "torch", "tornado", "tortoise", "toss",
    "total", "tourist", "toward", "tower", "town", "toy", "track", "trade", "traffic", "tragic",
    "train", "transfer", "trap", "trash", "travel", "tray", "treat", "tree", "trend", "trial",
    "tribe", "trick", "trigger", "trim", "trip", "trophy", "trouble", "truck", "true", "truly",
    "trumpet", "trust", "truth", "try", "tube", "tuition", "tumble", "tuna", "tunnel", "turkey",
    "turn", "turtle", "twelve", "twenty", "twice", "twin", "twist", "two", "type", "typical",
    "ugly", "umbrella", "unable", "unaware", "uncle", "uncover", "under", "undo", "unfair",
    "unfold", "unhappy", "uniform", "unique", "unit", "universe", "unknown", "unlock", "until",
    "unusual", "unveil", "update", "upgrade", "uphold", "upon", "upper", "upset", "urban", "urge",
    "usage", "use", "used", "useful", "useless", "usual", "utility", "vacant", "vacuum", "vague",
    "valid", "valley", "valve", "van", "vanish", "vapor", "various", "vast", "vault", "vehicle",
    "velvet", "vendor", "venture", "venue", "verb", "verify", "version", "very", "vessel",
    "veteran", "viable", "vibrant", "vicious", "victory", "video", "view", "village", "vintage",
    "violin", "virtual", "virus", "visa", "visit", "visual", "vital", "vivid", "vocal", "voice",
    "void", "volcano", "volume", "vote", "voyage", "wage", "wagon", "wait", "walk", "wall",
    "walnut", "want", "warfare", "warm", "warrior", "wash", "wasp", "waste", "water", "wave",
    "way", "wealth", "weapon", "wear", "weasel", "weather", "web", "wedding", "weekend", "weird",
    "welcome", "west", "wet", "whale", "what", "wheat", "wheel", "when", "where", "whip",
    "whisper", "wide", "width", "wife", "wild", "will", "win", "window", "wine", "wing", "wink",
    "winner", "winter", "wire", "wisdom", "wise", "wish", "witness", "wolf", "woman", "wonder",
    "wood", "wool", "word", "work", "world", "worry", "worth", "wrap", "wreck", "wrestle", "wrist",
    "write", "wrong", "yard", "year", "yellow", "you", "young", "youth", "zebra", "zero", "zone",
    "zoo",
];

/// PBKDF2-HMAC-SHA512 (RFC 8018 §5.2, `dkLen = 64`, `c = 2048`).
/// Used by `mnemonic_to_seed`; the BIP-39 32-byte ed25519 seed is the
/// **first 32 bytes** of the 64-byte output. The actual PBKDF2 is the
/// audited `pbkdf2` crate (M169) — not hand-rolled, because XOR-accumulator
/// implementations of PBKDF2 are notoriously easy to get subtly wrong
/// (off-by-one block index, missing last partial block, etc.) and the
/// BIP-39 spec offers no slack for "approximately correct" derivations.
fn pbkdf2_hmac_sha512(password: &[u8], salt: &[u8], iter_count: u32, out: &mut [u8; 64]) {
    *out = pbkdf2::pbkdf2_hmac_array::<sha2::Sha512, 64>(password, salt, iter_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let kp = Keypair::from_seed([7u8; 32]);
        let pk = kp.public();
        let msg = b"knowledge submission";
        let sig = kp.sign(msg);
        assert!(verify(&pk, msg, &sig));
    }

    #[test]
    fn tampered_message_fails() {
        let kp = Keypair::from_seed([7u8; 32]);
        let pk = kp.public();
        let sig = kp.sign(b"original");
        assert!(!verify(&pk, b"tampered", &sig));
    }

    #[test]
    fn wrong_key_fails() {
        let kp = Keypair::from_seed([1u8; 32]);
        let other = Keypair::from_seed([2u8; 32]).public();
        let msg = b"x";
        let sig = kp.sign(msg);
        assert!(!verify(&other, msg, &sig));
    }

    #[test]
    fn secret_seed_round_trips() {
        // The seed is exactly what `from_seed` consumes, so a round trip through
        // `secret_seed` reproduces the same key (M43 mTLS derives its credential
        // from this seed and must land on the same public key as consensus).
        let seed = [42u8; 32];
        let kp = Keypair::from_seed(seed);
        assert_eq!(kp.secret_seed(), seed);
        assert_eq!(Keypair::from_seed(kp.secret_seed()).public(), kp.public());
    }

    // ---- M169: BIP-39 mnemonic tests ----

    #[test]
    fn mnemonic_to_seed_known_vector() {
        // BIP-39 / Trezor test vector (vectors.json row 0). Per the
        // BIP-39 mediawiki, the passphrase "TREZOR" is used for **all**
        // vectors in that file (it's not optional), so the "empty"
        // passphrase case in Trezor is not tested there.
        //
        // Input:
        //   words      = "abandon abandon ... abandon about"  (12 words)
        //   passphrase = "TREZOR"
        //   salt       = b"mnemonic" + b"TREZOR"  = b"mnemonicTREZOR"
        //   PBKDF2-HMAC-SHA512 × 2048 → 64 bytes; we use the first 32.
        let words = "abandon abandon abandon abandon abandon abandon \
                     abandon abandon abandon abandon abandon about";
        let mut out = [0u8; 32];
        mnemonic_to_seed(words, "TREZOR", &mut out).unwrap();
        assert_eq!(
            hex(&out),
            "c55257c360c07c72029aebc1b53c05ed0362ada38ead3e3e9efa3708e5349553"
        );
    }

    #[test]
    fn mnemonic_to_seed_with_passphrase_changes_seed() {
        // Prove PBKDF2 actually folds the passphrase into the salt — same
        // words with empty vs. non-empty passphrase must produce different
        // seeds, and the derivation must be deterministic. We don't pin a
        // golden value (the BIP-39 Trezor vectors only test passphrase
        // "TREZOR" which the prior test exercises) — the structural
        // property is what we want to guard.
        let words = "abandon abandon abandon abandon abandon abandon \
                     abandon abandon abandon abandon abandon about";
        let mut no_pass = [0u8; 32];
        let mut with_pass = [0u8; 32];
        mnemonic_to_seed(words, "", &mut no_pass).unwrap();
        mnemonic_to_seed(words, "another-pass", &mut with_pass).unwrap();
        assert_ne!(
            no_pass, with_pass,
            "passphrase must change the seed (BIP-39 25th-word salt)"
        );
        // Determinism: re-deriving with the same passphrase yields the same bytes.
        let mut with_pass2 = [0u8; 32];
        mnemonic_to_seed(words, "another-pass", &mut with_pass2).unwrap();
        assert_eq!(with_pass, with_pass2);
    }

    #[test]
    fn mnemonic_round_trip_generate_validate_derive() {
        // Generate 24-word mnemonic from random entropy, validate it,
        // re-derive the seed, and assert the derived keypair's public key
        // matches the seed's expected pubkey. End-to-end BIP-39 round trip.
        let mut entropy = [0u8; 32];
        for (i, b) in entropy.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let words = mnemonic_generate(&entropy).expect("generate");
        assert_eq!(words.split_whitespace().count(), 24);
        validate_mnemonic(&words).expect("validate after generate");
        let mut seed = [0u8; 32];
        mnemonic_to_seed(&words, "", &mut seed).unwrap();
        // PBKDF2 is deterministic, so re-derive from the words and the
        // known-to-not-be-the-original seed must match exactly only if the
        // mnemonic is self-consistent. We don't claim the seed equals the
        // input entropy (it shouldn't — entropy is the source, seed is the
        // PBKDF2 stretch). Instead, we re-derive twice and assert both
        // derivations agree.
        let mut seed2 = [0u8; 32];
        mnemonic_to_seed(&words, "", &mut seed2).unwrap();
        assert_eq!(seed, seed2);
        // And the keypair derived from that seed signs+verifies.
        let kp = Keypair::from_seed(seed);
        let sig = kp.sign(b"m169 round trip");
        assert!(verify(&kp.public(), b"m169 round trip", &sig));
    }

    #[test]
    fn validate_mnemonic_rejects_bad_inputs() {
        // Wrong length.
        let bad_len = "abandon ".repeat(13);
        assert_eq!(
            validate_mnemonic(bad_len.trim_end()),
            Err(MnemonicError::InvalidLength(13))
        );
        // Unknown word.
        assert!(matches!(
            validate_mnemonic(
                "abandon abandon abandon abandon abandon abandon \
                                abandon abandon abandon abandon abandon notaword"
            ),
            Err(MnemonicError::UnknownWord(_))
        ));
        // Bad checksum: last word flipped from "about" (idx 102) to
        // "ability" (idx 1) ⇒ SHA256(0*16)[0] >> 4 = 3 but the
        // mnemonic's last-4-checksum bits = 1.
        assert!(matches!(
            validate_mnemonic(
                "abandon abandon abandon abandon abandon abandon \
                                abandon abandon abandon abandon abandon ability"
            ),
            Err(MnemonicError::BadChecksum)
        ));
        // Non-ASCII rejected (M169 ASCII-only).
        assert_eq!(validate_mnemonic("café"), Err(MnemonicError::NonAscii));
    }

    #[test]
    fn mnemonic_to_seed_rejects_non_ascii_passphrase() {
        // M169 ASCII-only: non-ASCII passphrase is rejected up front (no
        // PBKDF2 invocation, no half-derive).
        let words = "abandon abandon abandon abandon abandon abandon \
                     abandon abandon abandon abandon abandon about";
        let mut out = [0u8; 32];
        assert_eq!(
            mnemonic_to_seed(words, "пароль", &mut out),
            Err(MnemonicError::NonAscii)
        );
        // The out buffer must be unchanged (zero by caller construction).
        assert_eq!(out, [0u8; 32]);
    }
}
