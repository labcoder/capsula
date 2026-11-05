//! sherpa-onnx v2.0.1's offline recognizer structs, transcribed field for field from its
//! `c-api.h`. The header is the authority: the size, alignment and offset assertions below pin
//! this transcription, and `measure_sherpa_layout` checks them against the header on Rez.

use std::ffi::c_char;

type Str = *const c_char;

#[repr(C)]
pub(super) struct RawRecognizer {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(super) struct RawStream {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(super) struct FeatureConfig {
    pub sample_rate: i32,
    pub feature_dim: i32,
}

#[repr(C)]
pub(super) struct TransducerModelConfig {
    pub encoder: Str,
    pub decoder: Str,
    pub joiner: Str,
}

/// Every model config that is a single `const char *model`.
#[repr(C)]
pub(super) struct SingleModelConfig {
    pub model: Str,
}

#[repr(C)]
pub(super) struct WhisperModelConfig {
    pub encoder: Str,
    pub decoder: Str,
    pub language: Str,
    pub task: Str,
    pub tail_paddings: i32,
    pub enable_token_timestamps: i32,
    pub enable_segment_timestamps: i32,
}

#[repr(C)]
pub(super) struct SenseVoiceModelConfig {
    pub model: Str,
    pub language: Str,
    pub use_itn: i32,
}

#[repr(C)]
pub(super) struct MoonshineModelConfig {
    pub preprocessor: Str,
    pub encoder: Str,
    pub uncached_decoder: Str,
    pub cached_decoder: Str,
    pub merged_decoder: Str,
}

#[repr(C)]
pub(super) struct FireRedAsrModelConfig {
    pub encoder: Str,
    pub decoder: Str,
}

#[repr(C)]
pub(super) struct CanaryModelConfig {
    pub encoder: Str,
    pub decoder: Str,
    pub src_lang: Str,
    pub tgt_lang: Str,
    pub use_pnc: i32,
}

#[repr(C)]
pub(super) struct FunAsrNanoModelConfig {
    pub encoder_adaptor: Str,
    pub llm: Str,
    pub embedding: Str,
    pub tokenizer: Str,
    pub system_prompt: Str,
    pub user_prompt: Str,
    pub max_new_tokens: i32,
    pub temperature: f32,
    pub top_p: f32,
    pub seed: i32,
    pub language: Str,
    pub itn: i32,
    pub hotwords: Str,
}

#[repr(C)]
pub(super) struct Qwen3AsrModelConfig {
    pub conv_frontend: Str,
    pub encoder: Str,
    pub decoder: Str,
    pub tokenizer: Str,
    pub max_total_len: i32,
    pub max_new_tokens: i32,
    pub temperature: f32,
    pub top_p: f32,
    pub seed: i32,
    pub hotwords: Str,
}

#[repr(C)]
pub(super) struct CohereTranscribeModelConfig {
    pub encoder: Str,
    pub decoder: Str,
    pub language: Str,
    pub use_punct: i32,
    pub use_itn: i32,
}

#[repr(C)]
pub(super) struct ModelConfig {
    pub transducer: TransducerModelConfig,
    pub paraformer: SingleModelConfig,
    pub nemo_ctc: SingleModelConfig,
    pub whisper: WhisperModelConfig,
    pub tdnn: SingleModelConfig,
    pub tokens: Str,
    pub num_threads: i32,
    pub debug: i32,
    pub provider: Str,
    pub model_type: Str,
    pub modeling_unit: Str,
    pub bpe_vocab: Str,
    pub telespeech_ctc: Str,
    pub sense_voice: SenseVoiceModelConfig,
    pub moonshine: MoonshineModelConfig,
    pub fire_red_asr: FireRedAsrModelConfig,
    pub dolphin: SingleModelConfig,
    pub zipformer_ctc: SingleModelConfig,
    pub canary: CanaryModelConfig,
    pub wenet_ctc: SingleModelConfig,
    pub omnilingual: SingleModelConfig,
    pub medasr: SingleModelConfig,
    pub funasr_nano: FunAsrNanoModelConfig,
    pub fire_red_asr_ctc: SingleModelConfig,
    pub qwen3_asr: Qwen3AsrModelConfig,
    pub cohere_transcribe: CohereTranscribeModelConfig,
}

#[repr(C)]
pub(super) struct LmConfig {
    pub model: Str,
    pub scale: f32,
}

#[repr(C)]
pub(super) struct HomophoneReplacerConfig {
    pub dict_dir: Str,
    pub lexicon: Str,
    pub rule_fsts: Str,
}

#[repr(C)]
pub(super) struct CtcFstDecoderConfig {
    pub graph: Str,
    pub max_active: i32,
}

#[repr(C)]
pub(super) struct RecognizerConfig {
    pub feat_config: FeatureConfig,
    pub model_config: ModelConfig,
    pub lm_config: LmConfig,
    pub decoding_method: Str,
    pub max_active_paths: i32,
    pub hotwords_file: Str,
    pub hotwords_score: f32,
    pub rule_fsts: Str,
    pub rule_fars: Str,
    pub blank_penalty: f32,
    pub hr: HomophoneReplacerConfig,
    pub ctc_fst_decoder_config: CtcFstDecoderConfig,
}

#[repr(C)]
pub(super) struct RecognizerResult {
    pub text: Str,
    pub timestamps: *mut f32,
    pub count: i32,
    pub tokens: Str,
    pub tokens_arr: *const Str,
    pub json: Str,
    pub lang: Str,
    pub emotion: Str,
    pub event: Str,
    pub durations: *mut f32,
    pub ys_log_probs: *mut f32,
    pub segment_timestamps: *const f32,
    pub segment_durations: *const f32,
    pub segment_texts: Str,
    pub segment_texts_arr: *const Str,
    pub segment_count: i32,
}

/// Every size, alignment and offset Kumochi depends on, as `(name, value)`: the structs whole,
/// each field Kumochi writes, and `text`, the one it reads. `measure_sherpa_layout` prints the
/// same names from C.
pub(super) const LAYOUT: [(&str, usize); 16] = [
    (
        "sizeof(SherpaOnnxOfflineRecognizerConfig)",
        size_of::<RecognizerConfig>(),
    ),
    (
        "alignof(SherpaOnnxOfflineRecognizerConfig)",
        align_of::<RecognizerConfig>(),
    ),
    (
        "sizeof(SherpaOnnxOfflineModelConfig)",
        size_of::<ModelConfig>(),
    ),
    (
        "sizeof(SherpaOnnxOfflineRecognizerResult)",
        size_of::<RecognizerResult>(),
    ),
    (
        "alignof(SherpaOnnxOfflineRecognizerResult)",
        align_of::<RecognizerResult>(),
    ),
    (
        "offsetof(SherpaOnnxOfflineRecognizerConfig, model_config)",
        std::mem::offset_of!(RecognizerConfig, model_config),
    ),
    (
        "offsetof(SherpaOnnxOfflineRecognizerConfig, decoding_method)",
        std::mem::offset_of!(RecognizerConfig, decoding_method),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, transducer.encoder)",
        std::mem::offset_of!(ModelConfig, transducer.encoder),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, transducer.decoder)",
        std::mem::offset_of!(ModelConfig, transducer.decoder),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, transducer.joiner)",
        std::mem::offset_of!(ModelConfig, transducer.joiner),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, tokens)",
        std::mem::offset_of!(ModelConfig, tokens),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, num_threads)",
        std::mem::offset_of!(ModelConfig, num_threads),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, debug)",
        std::mem::offset_of!(ModelConfig, debug),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, provider)",
        std::mem::offset_of!(ModelConfig, provider),
    ),
    (
        "offsetof(SherpaOnnxOfflineModelConfig, model_type)",
        std::mem::offset_of!(ModelConfig, model_type),
    ),
    (
        "offsetof(SherpaOnnxOfflineRecognizerResult, text)",
        std::mem::offset_of!(RecognizerResult, text),
    ),
];

/// The values `LAYOUT` must have on x64, worked out from the header by hand.
const EXPECTED: [usize; 16] = [
    624, 8, 504, 128, 8, 8, 528, 0, 8, 16, 96, 104, 108, 112, 120, 0,
];

const _: () = {
    let mut i = 0;
    while i < LAYOUT.len() {
        assert!(
            LAYOUT[i].1 == EXPECTED[i],
            "the sherpa-onnx layout drifted from c-api.h"
        );
        i += 1;
    }
};
