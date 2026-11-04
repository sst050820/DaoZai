//! 提醒铃声：软件合成 WAV（跨平台可测），Windows 上用 PlaySound 播放。
//!
//! 不引入音频依赖：所有音色都由简单的加性合成算出 PCM，再套一个 44 字节的
//! RIFF 头。这样铃声与界面一样是「项目自带的一份代码」，不依赖系统提示音，
//! 用户换机器、换 Windows 版本听到的都是同一个声音。

use std::f32::consts::TAU;

const RATE: u32 = 44_100;

/// 可选的铃声风格。顺序与设置界面的选项一致，落盘时存的是下标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chime {
    /// 轻提示：两声柔和下行。
    Soft,
    /// 上扬：一声上滑。
    Rise,
    /// 铃铛：带泛音的金属音，最像「铃声」。
    Bell,
    /// 木鱼：一声短促的敲击。
    Wood,
    /// 水滴：圆润的下坠音。
    Water,
    /// 提示：两声清脆的提醒，最醒目。
    Alert,
}

/// 设置界面用的清单：(风格, 名称, 说明)。
pub const STYLES: [(Chime, &str, &str); 6] = [
    (Chime::Soft, "轻提示", "两声柔和的下行，最轻"),
    (Chime::Rise, "上扬", "一声上滑，像提示气泡"),
    (Chime::Bell, "铃铛", "带泛音的金属音，最像铃声"),
    (Chime::Wood, "木鱼", "一声短促的敲击"),
    (Chime::Water, "水滴", "圆润的下坠音"),
    (Chime::Alert, "提示", "两声清脆，最醒目"),
];

impl Chime {
    /// 从 settings.txt 的下标还原；越界时回到默认。
    pub fn from_index(value: u8) -> Self {
        match value {
            1 => Self::Rise,
            2 => Self::Bell,
            3 => Self::Wood,
            4 => Self::Water,
            5 => Self::Alert,
            _ => Self::Soft,
        }
    }

    pub fn index(self) -> u8 {
        match self {
            Self::Soft => 0,
            Self::Rise => 1,
            Self::Bell => 2,
            Self::Wood => 3,
            Self::Water => 4,
            Self::Alert => 5,
        }
    }

    pub fn label(self) -> &'static str {
        STYLES
            .iter()
            .find(|(style, _, _)| *style == self)
            .map(|(_, name, _)| *name)
            .unwrap_or("轻提示")
    }
}

impl Default for Chime {
    fn default() -> Self {
        // 默认给铃铛：用户要的是「铃声」，金属音最容易被认出来。
        Self::Bell
    }
}

/// 一个音符：起播时刻、时长、起止频率、音量、指数衰减速率、泛音表。
struct Note {
    at: f32,
    dur: f32,
    freq: f32,
    /// 为 0 表示音高不变。
    end_freq: f32,
    amp: f32,
    decay: f32,
    /// (泛音倍数, 相对音量)
    harmonics: &'static [(f32, f32)],
}

/// 泛音表：铃铛、柔和、木鱼、电子音各有不同的谐波构成，决定音色。
static BELL: [(f32, f32); 4] = [(1.0, 1.0), (2.0, 0.42), (3.01, 0.20), (4.20, 0.10)];
static SOFT: [(f32, f32); 2] = [(1.0, 1.0), (2.0, 0.18)];
static CLICK: [(f32, f32); 2] = [(1.0, 1.0), (2.0, 0.30)];
static BEEP: [(f32, f32); 2] = [(1.0, 1.0), (3.0, 0.35)];

/// 每个风格的乐谱与整段时长。
fn score(kind: Chime) -> (Vec<Note>, f32) {
    let bell = &BELL;
    let soft = &SOFT;
    let click = &CLICK;
    let beep = &BEEP;
    let notes = match kind {
        Chime::Soft => vec![
            Note { at: 0.0, dur: 0.60, freq: 783.99, end_freq: 0.0, amp: 0.45, decay: 6.0, harmonics: soft },
            Note { at: 0.15, dur: 0.60, freq: 587.33, end_freq: 0.0, amp: 0.40, decay: 6.0, harmonics: soft },
        ],
        Chime::Rise => vec![Note {
            at: 0.0, dur: 0.42, freq: 523.25, end_freq: 987.77, amp: 0.50, decay: 1.8, harmonics: soft,
        }],
        Chime::Bell => vec![
            Note { at: 0.0, dur: 1.35, freq: 880.0, end_freq: 0.0, amp: 0.40, decay: 3.2, harmonics: bell },
            Note { at: 0.0, dur: 1.35, freq: 1320.0, end_freq: 0.0, amp: 0.10, decay: 4.6, harmonics: soft },
        ],
        Chime::Wood => vec![
            Note { at: 0.0, dur: 0.18, freq: 440.0, end_freq: 320.0, amp: 0.55, decay: 24.0, harmonics: click },
            Note { at: 0.0, dur: 0.05, freq: 1600.0, end_freq: 0.0, amp: 0.12, decay: 60.0, harmonics: click },
        ],
        Chime::Water => vec![Note {
            at: 0.0, dur: 0.45, freq: 1180.0, end_freq: 470.0, amp: 0.50, decay: 7.0, harmonics: soft,
        }],
        Chime::Alert => vec![
            Note { at: 0.0, dur: 0.12, freq: 659.25, end_freq: 0.0, amp: 0.42, decay: 3.0, harmonics: beep },
            Note { at: 0.22, dur: 0.14, freq: 880.0, end_freq: 0.0, amp: 0.42, decay: 3.0, harmonics: beep },
        ],
    };
    let total = notes
        .iter()
        .map(|note| note.at + note.dur)
        .fold(0.0f32, f32::max)
        + 0.03; // 尾巴：避免截断产生爆音
    (notes, total)
}

fn sample(notes: &[Note], t: f32) -> f32 {
    let mut sum = 0.0;
    for note in notes {
        let local = t - note.at;
        if local < 0.0 || local > note.dur {
            continue;
        }
        // 起音 6ms 淡入、收音 20ms 淡出、中间指数衰减：都是为了避免爆音。
        let envelope = (local / 0.006).min(1.0)
            * (-local * note.decay).exp()
            * ((note.dur - local) / 0.02).min(1.0);
        let freq = if note.end_freq > 0.0 {
            note.freq + (note.end_freq - note.freq) * (local / note.dur)
        } else {
            note.freq
        };
        let phase = TAU * freq * local;
        let mut voice = 0.0;
        for &(multiple, level) in note.harmonics {
            voice += level * (phase * multiple).sin();
        }
        sum += envelope * note.amp * voice;
    }
    sum
}

/// 合成一段完整的 WAV（16 位单声道 44.1kHz）。
///
/// volume 取 0..=100 的百分比；超出范围会被夹住。
pub fn wav(kind: Chime, volume: u32) -> Vec<u8> {
    let (notes, total) = score(kind);
    let gain = volume.min(100) as f32 / 100.0 * 0.9;
    let count = (total * RATE as f32) as usize;
    let mut pcm = Vec::with_capacity(count);
    for index in 0..count {
        let t = index as f32 / RATE as f32;
        let value = (sample(&notes, t) * gain).clamp(-1.0, 1.0);
        pcm.push((value * 32767.0) as i16);
    }
    encode(&pcm)
}

fn encode(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM 头长度
    out.extend_from_slice(&1u16.to_le_bytes()); // 未压缩 PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // 字节率
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for value in pcm {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// 播放一次铃声。音量在合成阶段就已经缩放，不依赖系统音量条。
#[cfg(windows)]
pub fn play(kind: Chime, volume: u32) {
    use std::sync::{Mutex, OnceLock};
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_PURGE};

    // PlaySound 的 SND_ASYNC 会自己起线程读这块内存，所以缓冲区必须比调用活得更久。
    static HOLD: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
    let hold = HOLD.get_or_init(|| Mutex::new(Vec::new()));
    let mut buf = hold.lock().unwrap_or_else(|e| e.into_inner());
    // 先停掉上一次的播放，再替换缓冲区：否则旧内存可能在播放线程读取时被释放。
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, SND_PURGE);
    }
    buf.clear();
    buf.extend_from_slice(&wav(kind, volume));
    let start = buf.as_ptr() as *const u16;
    let played = unsafe {
        PlaySoundW(PCWSTR(start), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT).as_bool()
    };
    drop(buf);
    // 播放失败通常是「没有可用音频设备」或设备被独占；只记一条诊断，
    // 不弹窗打断用户 —— 提醒本身已经在岛上了。
    if !played {
        let directory = crate::model::config_dir().join("diagnostics");
        if std::fs::create_dir_all(&directory).is_ok() {
            let _ = std::fs::write(
                directory.join("last-chime.log"),
                format!("铃声播放失败：{kind:?} 音量 {volume}\n"),
            );
        }
    }
}

/// 非 Windows（录制器、测试）不需要真的出声。
#[cfg(not(windows))]
pub fn play(_kind: Chime, _volume: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_synthesizes_a_valid_wav() {
        for (kind, name, _) in STYLES {
            let bytes = wav(kind, 80);
            assert_eq!(&bytes[..4], b"RIFF", "{name} 缺 RIFF 头");
            assert_eq!(&bytes[8..12], b"WAVE", "{name} 缺 WAVE 标记");
            assert_eq!(&bytes[36..40], b"data", "{name} 缺 data 块");
            let declared = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
            assert_eq!(declared, bytes.len() - 44, "{name} 的 data 长度与实际不符");
            assert!(declared > 4_000, "{name} 太短：{declared} 字节");
        }
    }

    #[test]
    fn volume_scales_the_waveform() {
        let loud = wav(Chime::Bell, 100);
        let quiet = wav(Chime::Bell, 30);
        let peak = |bytes: &[u8]| {
            bytes[44..]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
                .max()
                .unwrap_or(0)
        };
        assert!(peak(&loud) > peak(&quiet) * 2, "音量 100 的峰值应明显高于音量 30");
        assert!(peak(&quiet) > 0, "音量 30 不应静音");
    }

    #[test]
    fn style_index_round_trips_and_rejects_out_of_range() {
        for (kind, _, _) in STYLES {
            assert_eq!(Chime::from_index(kind.index()), kind);
        }
        assert_eq!(Chime::from_index(200), Chime::Soft);
    }

    #[test]
    fn synthesized_audio_starts_and_ends_near_silence() {
        // 首尾都淡入淡出，接在别的提示音后面才不会「啪」一下。
        let bytes = wav(Chime::Alert, 100);
        let first = i16::from_le_bytes([bytes[44], bytes[45]]).unsigned_abs();
        let last_pair = &bytes[bytes.len() - 2..];
        let last = i16::from_le_bytes([last_pair[0], last_pair[1]]).unsigned_abs();
        assert!(first < 2_000, "起始样本过大：{first}");
        assert!(last < 2_000, "结尾样本过大：{last}");
    }

    #[test]
    #[ignore = "导出六种铃声到目录，供试听与波形核对"]
    fn dump_chime_wavs() {
        let dest = std::path::PathBuf::from(
            std::env::var_os("DAOZAI_CHIME_WAV_DIR").expect("需指定导出目录"),
        );
        std::fs::create_dir_all(&dest).unwrap();
        for (kind, name, _) in STYLES {
            let bytes = wav(kind, 85);
            let seconds = (bytes.len() - 44) as f32 / 2.0 / RATE as f32;
            let peak = bytes[44..]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
                .max()
                .unwrap_or(0);
            println!("{name}	{seconds:.2}s	{} 字节	峰值 {peak}", bytes.len());
            std::fs::write(dest.join(format!("{}-{name}.wav", kind.index())), bytes).unwrap();
        }
    }
}
