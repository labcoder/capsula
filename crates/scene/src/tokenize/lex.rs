//! Incremental JSON scalar syntax; no decoded drawing strings are retained.

#[derive(Debug, Clone, Copy)]
pub(super) enum Lex {
    None,
    String(StringState),
    Number(Number),
    Literal { word: &'static [u8], offset: u8 },
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Purpose {
    RootKey,
    Key,
    Value,
}

#[derive(Debug, Clone, Copy)]
enum Escape {
    None,
    Slash,
    Hex { value: u16, digits: u8, low: bool },
    LowSlash,
    LowU,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct StringState {
    pub(super) purpose: Purpose,
    escape: Escape,
    remaining: u8,
    min: u8,
    max: u8,
}

impl StringState {
    pub(super) fn new(purpose: Purpose) -> Self {
        Self {
            purpose,
            escape: Escape::None,
            remaining: 0,
            min: 0x80,
            max: 0xbf,
        }
    }

    pub(super) fn byte(&mut self, byte: u8, key_len: &mut usize) -> Result<bool, ()> {
        if self.remaining > 0 {
            if !(self.min..=self.max).contains(&byte) {
                return Err(());
            }
            self.remaining -= 1;
            self.min = 0x80;
            self.max = 0xbf;
            return Ok(false);
        }
        match self.escape {
            Escape::Slash => {
                self.escape = Escape::None;
                let decoded = match byte {
                    b'"' | b'\\' | b'/' => byte,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => {
                        self.escape = Escape::Hex {
                            value: 0,
                            digits: 0,
                            low: false,
                        };
                        return Ok(false);
                    }
                    _ => return Err(()),
                };
                self.decoded(u16::from(decoded), key_len)?;
            }
            Escape::Hex { value, digits, low } => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => return Err(()),
                };
                let value = (value << 4) | u16::from(digit);
                if digits < 3 {
                    self.escape = Escape::Hex {
                        value,
                        digits: digits + 1,
                        low,
                    };
                } else if low {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return Err(());
                    }
                    self.escape = Escape::None;
                    self.decoded(0xffff, key_len)?;
                } else if (0xd800..=0xdbff).contains(&value) {
                    self.escape = Escape::LowSlash;
                } else {
                    if (0xdc00..=0xdfff).contains(&value) {
                        return Err(());
                    }
                    self.escape = Escape::None;
                    self.decoded(value, key_len)?;
                }
            }
            Escape::LowSlash => {
                if byte != b'\\' {
                    return Err(());
                }
                self.escape = Escape::LowU;
            }
            Escape::LowU => {
                if byte != b'u' {
                    return Err(());
                }
                self.escape = Escape::Hex {
                    value: 0,
                    digits: 0,
                    low: true,
                };
            }
            Escape::None => match byte {
                b'"' => {
                    if matches!(self.purpose, Purpose::RootKey) && *key_len != 7 {
                        return Err(());
                    }
                    return Ok(true);
                }
                b'\\' => self.escape = Escape::Slash,
                0x00..=0x1f => return Err(()),
                0x20..=0x7f => self.decoded(u16::from(byte), key_len)?,
                _ => self.start_utf8(byte)?,
            },
        }
        Ok(false)
    }

    fn start_utf8(&mut self, byte: u8) -> Result<(), ()> {
        if matches!(self.purpose, Purpose::RootKey) {
            return Err(());
        }
        let (remaining, min, max) = match byte {
            0xc2..=0xdf => (1, 0x80, 0xbf),
            0xe0 => (2, 0xa0, 0xbf),
            0xed => (2, 0x80, 0x9f),
            0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf),
            0xf0 => (3, 0x90, 0xbf),
            0xf4 => (3, 0x80, 0x8f),
            0xf1..=0xf3 => (3, 0x80, 0xbf),
            _ => return Err(()),
        };
        self.remaining = remaining;
        self.min = min;
        self.max = max;
        Ok(())
    }

    fn decoded(self, value: u16, key_len: &mut usize) -> Result<(), ()> {
        if matches!(self.purpose, Purpose::RootKey) {
            if b"records".get(*key_len).copied().map(u16::from) != Some(value) {
                return Err(());
            }
            *key_len += 1;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Number {
    Minus,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exp,
    ExpSign,
    ExpDigits,
}

impl Number {
    pub(super) fn byte(self, byte: u8) -> Result<Option<Self>, ()> {
        let next = match (self, byte) {
            (Self::Minus, b'0') => Self::Zero,
            (Self::Minus, b'1'..=b'9') | (Self::Integer, b'0'..=b'9') => Self::Integer,
            (Self::Zero | Self::Integer, b'.') => Self::Dot,
            (Self::Dot | Self::Fraction, b'0'..=b'9') => Self::Fraction,
            (Self::Zero | Self::Integer | Self::Fraction, b'e' | b'E') => Self::Exp,
            (Self::Exp, b'+' | b'-') => Self::ExpSign,
            (Self::Exp | Self::ExpSign | Self::ExpDigits, b'0'..=b'9') => Self::ExpDigits,
            (
                Self::Zero | Self::Integer | Self::Fraction | Self::ExpDigits,
                b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}',
            ) => return Ok(None),
            _ => return Err(()),
        };
        Ok(Some(next))
    }
}
