use std::{error::Error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Maximum UTF-8 byte length of every persisted identifier.
pub const MAX_IDENTIFIER_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentifierError {
    Empty,
    TooLong { actual: usize, maximum: usize },
    InvalidCharacter { index: usize, character: char },
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("identifier must not be empty"),
            Self::TooLong { actual, maximum } => {
                write!(
                    formatter,
                    "identifier is {actual} bytes; maximum is {maximum} bytes"
                )
            }
            Self::InvalidCharacter { index, character } => write!(
                formatter,
                "identifier contains invalid character {character:?} at byte {index}"
            ),
        }
    }
}

impl Error for IdentifierError {}

fn validate_identifier(value: &str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty);
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(IdentifierError::TooLong {
            actual: value.len(),
            maximum: MAX_IDENTIFIER_BYTES,
        });
    }

    for (index, character) in value.char_indices() {
        if !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.' | ':') {
            return Err(IdentifierError::InvalidCharacter { index, character });
        }
    }

    Ok(())
}

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, IdentifierError> {
                let value = value.as_ref();
                validate_identifier(value)?;
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

identifier!(SessionId);
identifier!(DocumentId);
identifier!(CommandId);

/// A fixed 32-byte digest, serialized as exactly 64 lowercase hexadecimal bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Hash([u8; 32]);

impl Hash {
    pub const LENGTH: usize = 32;
    pub const ENCODED_LENGTH: usize = Self::LENGTH * 2;

    pub const fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }

    pub const fn zero() -> Self {
        Self([0; Self::LENGTH])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HashError {
    InvalidLength { actual: usize, expected: usize },
    InvalidCharacter { index: usize, character: char },
}

impl fmt::Display for HashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { actual, expected } => write!(
                formatter,
                "hash is {actual} bytes; expected exactly {expected} lowercase hexadecimal bytes"
            ),
            Self::InvalidCharacter { index, character } => write!(
                formatter,
                "hash contains non-canonical character {character:?} at byte {index}"
            ),
        }
    }
}

impl Error for HashError {}

impl FromStr for Hash {
    type Err = HashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != Self::ENCODED_LENGTH {
            return Err(HashError::InvalidLength {
                actual: value.len(),
                expected: Self::ENCODED_LENGTH,
            });
        }

        let mut bytes = [0; Self::LENGTH];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = decode_hex(pair[0], index * 2)?;
            let low = decode_hex(pair[1], index * 2 + 1)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn decode_hex(byte: u8, index: usize) -> Result<u8, HashError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(HashError::InvalidCharacter {
            index,
            character: char::from(byte),
        }),
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = [0; Hash::ENCODED_LENGTH];
        for (index, byte) in self.0.iter().copied().enumerate() {
            encoded[index * 2] = HEX[usize::from(byte >> 4)];
            encoded[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        let encoded = std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?;
        formatter.write_str(encoded)
    }
}

impl Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_round_trip() {
        let bytes = std::array::from_fn(|index| index as u8);
        let hash = Hash::from_bytes(bytes);
        assert_eq!(hash.to_string().parse::<Hash>().unwrap(), hash);
        assert_eq!(hash.as_bytes(), &bytes);
    }
}
