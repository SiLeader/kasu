use crate::{Canonical, Decoder, Encoder, GitOid, Result, invalid};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitKind {
    Blob,
    Tree,
    Commit,
    Tag,
}
impl GitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}
impl Canonical for GitKind {
    fn encode_to(&self, out: &mut Encoder) {
        out.byte(match self {
            Self::Blob => 0,
            Self::Tree => 1,
            Self::Commit => 2,
            Self::Tag => 3,
        });
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        match input.byte()? {
            0 => Ok(Self::Blob),
            1 => Ok(Self::Tree),
            2 => Ok(Self::Commit),
            3 => Ok(Self::Tag),
            _ => Err(invalid("invalid Git object kind")),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitRef {
    Direct {
        target: GitOid,
        peeled: Option<GitOid>,
        kind: GitKind,
    },
    Symbolic {
        target: Vec<u8>,
    },
}
impl Canonical for GitRef {
    fn encode_to(&self, out: &mut Encoder) {
        match self {
            Self::Direct {
                target,
                peeled,
                kind,
            } => {
                out.byte(0);
                target.encode_to(out);
                kind.encode_to(out);
                match peeled {
                    None => out.byte(0),
                    Some(oid) => {
                        out.byte(1);
                        oid.encode_to(out);
                    }
                }
            }
            Self::Symbolic { target } => {
                out.byte(1);
                out.bytes(target);
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        match input.byte()? {
            0 => {
                let target = GitOid::decode_from(input)?;
                let kind = GitKind::decode_from(input)?;
                let peeled = match input.byte()? {
                    0 => None,
                    1 => Some(GitOid::decode_from(input)?),
                    _ => return Err(invalid("invalid peeled tag")),
                };
                Ok(Self::Direct {
                    target,
                    peeled,
                    kind,
                })
            }
            1 => Ok(Self::Symbolic {
                target: input.bytes()?.to_vec(),
            }),
            _ => Err(invalid("invalid Git ref")),
        }
    }
}
