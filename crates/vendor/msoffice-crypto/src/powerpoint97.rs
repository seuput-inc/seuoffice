#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! PowerPoint 97-2003 (`.ppt`) decryption — [MS-PPT] §2.3.7 `CryptSession10Container`,
//! over RC4 CryptoAPI ([MS-OFFCRYPTO] §2.3.5), the only family the format defines.
//!
//! Persist objects are decrypted with their identifiers as RC4 block numbers.
//! `Pictures` and document-property streams are retained unchanged, as in
//! officecrypto-tool. The persist directory remains intact: decrementing its
//! first entry as officecrypto-tool does can produce a file PowerPoint rejects.

use crate::binary_office::{
    self, UserEditAtom, CURRENT_USER, CURRENT_USER_HEADER_TOKEN_AT, POWERPOINT_DOCUMENT,
    USER_EDIT_ATOM_LEN_ENCRYPTED, USER_EDIT_ATOM_LEN_PLAIN,
};
use crate::error::Error;
use crate::file_params::{bad, wide, FileField, FileProblem, Invariant};
#[cfg(test)]
use crate::file_params::{AllowedSet, Bound};
use crate::legacy_container::LegacyContainer;
use crate::rc4;
use crate::rc4::BlockKeySchedule;
use crate::rc4_cryptoapi::{self, CryptoApiKeySchedule};

/// `CurrentUserAtom.headerToken` for a file that is not encrypted — [MS-PPT] §2.3.2.
const HEADER_TOKEN_PLAIN: u32 = 0xE391_C05F;

/// `OfficeArtFBSE` — [MS-ODRAW] §2.2.32.
#[cfg(test)]
const RT_FBSE: u16 = 0xF007;
/// `rgbUid1` / `rgbUid2` — [MS-ODRAW] §2.2.24-§2.2.31.
#[cfg(test)]
const UID_LEN: usize = 16;
/// `OfficeArtMetafileHeader` — [MS-ODRAW] §2.2.31: cbSize(4) rcBounds(16) ptSize(8)
/// cbSave(4) compression(1) filter(1).
#[cfg(test)]
const METAFILE_HEADER_LEN: usize = 34;
/// A bitmap blip's `tag` — [MS-ODRAW] §2.2.27-§2.2.30.
#[cfg(test)]
const BITMAP_TAG_LEN: usize = 1;
/// `RecordHeader` — [MS-ODRAW] §2.2.1.
#[cfg(test)]
const RECORD_HEADER_LEN: usize = 8;

/// A file offset that does not fit this host's `usize`: ours to report (#47).
fn unrepresentable(_: std::num::TryFromIntError) -> Error {
    Error::InvariantViolated(Invariant::Unrepresentable)
}

/// Decrypt the container in place. The container is left untouched on any error.
pub(crate) fn decrypt(container: &mut LegacyContainer, password: &str) -> Result<(), Error> {
    let current_user = container.read(CURRENT_USER)?;
    let doc = container.read(POWERPOINT_DOCUMENT)?;
    let mut src: &[u8] = &doc;

    let edit_offset = binary_office::current_edit_offset(&current_user).ok_or(
        Error::MissingStream("Current User stream shorter than its atom"),
    )?;
    let edit_offset = usize::try_from(edit_offset).map_err(unrepresentable)?;
    let atom = binary_office::user_edit_atom(&mut src, edit_offset as u64).ok_or_else(|| {
        bad(
            FileField::UserEditAtom,
            FileProblem::Unreadable {
                offset: wide(edit_offset),
            },
        )
    })?;
    if atom.rec_len == USER_EDIT_ATOM_LEN_PLAIN {
        return Err(Error::NotEncrypted);
    }
    debug_assert_eq!(atom.rec_len, USER_EDIT_ATOM_LEN_ENCRYPTED);
    let directory_offset =
        usize::try_from(atom.offset_persist_directory).map_err(unrepresentable)?;
    let directory = binary_office::persist_directory(&mut src, directory_offset as u64)
        .ok_or_else(|| {
            bad(
                FileField::PersistDirectory,
                FileProblem::Unreadable {
                    offset: wide(directory_offset),
                },
            )
        })?;
    let session_id = atom
        .encrypt_session_persist_id_ref
        .ok_or_else(|| bad(FileField::EncryptSessionPersistIdRef, FileProblem::Missing))?;
    let session_offset = *directory.get(&session_id).ok_or_else(|| {
        bad(
            FileField::EncryptSessionPersistIdRef,
            FileProblem::Unrecognized {
                got: session_id.into(),
            },
        )
    })?;
    let structure = binary_office::crypt_session_container(&mut src, u64::from(session_offset))
        .ok_or_else(|| {
            bad(
                FileField::CryptSession10Container,
                FileProblem::Unreadable {
                    offset: session_offset.into(),
                },
            )
        })?;
    let session_len = binary_office::record_header(&mut src, u64::from(session_offset))
        .map(|rh| rh.rec_len)
        .unwrap_or(0);

    let header = rc4_cryptoapi::parse_ppt(&structure)?;
    let schedule = CryptoApiKeySchedule::new(password, &header.salt, header.key_bits)?;
    schedule.verify(&header)?;

    // Persist objects may also follow the directory. Stop at the next object,
    // metadata, or end of stream rather than assuming the directory is last.
    let mut objects: Vec<(u32, usize)> = Vec::with_capacity(directory.len());
    for (&id, &offset) in &directory {
        objects.push((id, usize::try_from(offset).map_err(unrepresentable)?));
    }
    objects.sort_by_key(|&(_, offset)| offset);

    let mut plain = doc.clone();
    decrypt_objects(
        &schedule,
        &mut plain,
        &objects,
        directory_offset,
        edit_offset,
        usize::try_from(session_offset).map_err(unrepresentable)?,
        usize::try_from(session_len).map_err(unrepresentable)?,
    )?;

    // UserEditAtom: recLen 0x20 -> 0x1C, and the reference field zeroed where it stood.
    let rec_len_at = edit_offset + UserEditAtom::REC_LEN_AT;
    plain[rec_len_at..rec_len_at + 4].copy_from_slice(&USER_EDIT_ATOM_LEN_PLAIN.to_le_bytes());
    let ref_at = edit_offset + UserEditAtom::ENCRYPT_SESSION_REF_AT;
    plain[ref_at..ref_at + 4].fill(0);

    // The persist directory is deliberately not touched -- see the module docs for the
    // four bytes msoffcrypto changes here and what PowerPoint makes of them.

    // The container guarantees the same length; the header token is the one field of
    // `Current User` that changes, and only if it was the encrypted token.
    let mut current_user_plain = current_user.clone();
    if let Some(slot) =
        current_user_plain.get_mut(CURRENT_USER_HEADER_TOKEN_AT..CURRENT_USER_HEADER_TOKEN_AT + 4)
    {
        slot.copy_from_slice(&HEADER_TOKEN_PLAIN.to_le_bytes());
    }

    container.overwrite(POWERPOINT_DOCUMENT, &plain)?;
    if current_user_plain != current_user {
        container.overwrite(CURRENT_USER, &current_user_plain)?;
    }
    Ok(())
}

fn decrypt_objects(
    schedule: &dyn BlockKeySchedule,
    plain: &mut [u8],
    objects: &[(u32, usize)],
    directory_offset: usize,
    edit_offset: usize,
    session_offset: usize,
    session_len: usize,
) -> Result<(), Error> {
    for (i, &(id, offset)) in objects.iter().enumerate() {
        if offset > plain.len() {
            return Err(bad(
                FileField::PersistObject,
                FileProblem::Unreadable {
                    offset: wide(offset),
                },
            ));
        }
        if offset == directory_offset || offset == edit_offset {
            continue;
        }
        let next = objects.get(i + 1).map(|&(_, next)| next);
        let end = object_end(offset, next, plain.len(), directory_offset, edit_offset);
        if offset == session_offset {
            let zero_end = offset
                .saturating_add(8)
                .saturating_add(session_len)
                .min(plain.len());
            plain[offset..zero_end].fill(0);
        } else if end > offset {
            rc4::decrypt_with_block(schedule, &mut plain[offset..end], id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod object_tests {
    use super::*;

    #[test]
    fn decrypts_objects_on_both_sides_of_metadata() {
        let schedule = CryptoApiKeySchedule::new("synthetic", &[0; 16], 128).unwrap();
        let original: Vec<u8> = (0..64).collect();
        let mut encrypted = original.clone();
        rc4::decrypt_with_block(&schedule, &mut encrypted[0..16], 1).unwrap();
        rc4::decrypt_with_block(&schedule, &mut encrypted[48..64], 2).unwrap();
        decrypt_objects(&schedule, &mut encrypted, &[(1, 0), (2, 48)], 16, 32, 64, 0).unwrap();
        assert_eq!(encrypted, original);
    }
}

fn object_end(
    offset: usize,
    next: Option<usize>,
    len: usize,
    directory: usize,
    edit: usize,
) -> usize {
    let mut end = next.unwrap_or(len).min(len);
    for boundary in [directory, edit] {
        if boundary > offset {
            end = end.min(boundary);
        }
    }
    end
}

/// Decrypt a `Pictures` stream in place, record by record and field by field (module
/// docs). Every record must be a blip [MS-ODRAW] defines and must fit the stream; the
/// walk advances by at least the 8-byte header each time, so it ends.
#[cfg(test)]
pub(crate) fn decrypt_pictures(
    schedule: &dyn BlockKeySchedule,
    pictures: &mut [u8],
) -> Result<(), Error> {
    let len = pictures.len();
    let mut at = 0usize;
    let mut index = 0usize;
    while at < len {
        let header = pictures
            .get_mut(at..at + RECORD_HEADER_LEN)
            .ok_or_else(|| {
                let _ = index;
                bad(
                    FileField::PicturesRecord,
                    FileProblem::Truncated {
                        have: wide(len - at),
                        need: wide(RECORD_HEADER_LEN),
                    },
                )
            })?;
        rc4::decrypt_with_block(schedule, header, 0)?;
        let ver_instance = u16::from_le_bytes([header[0], header[1]]);
        let rec_type = u16::from_le_bytes([header[2], header[3]]);
        let rec_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

        let fields = blip_fields(rec_type, ver_instance)?;
        let body_start = at + RECORD_HEADER_LEN;
        let body_end = usize::try_from(rec_len)
            .ok()
            .and_then(|n| body_start.checked_add(n))
            .filter(|&end| end <= len)
            .ok_or_else(|| {
                bad(
                    FileField::PicturesRecord,
                    FileProblem::OutOfBounds {
                        offset: wide(body_start),
                        len: rec_len.into(),
                        end: wide(len),
                    },
                )
            })?;
        let body = pictures
            .get_mut(body_start..body_end)
            .ok_or(Error::InvariantViolated(Invariant::InternalSlice))?;
        let mut field_start = 0usize;
        for size in fields {
            let field_end = field_start + size;
            let field = body.get_mut(field_start..field_end).ok_or_else(|| {
                bad(
                    FileField::PicturesRecord,
                    FileProblem::Truncated {
                        have: rec_len.into(),
                        need: wide(field_end),
                    },
                )
            })?;
            rc4::decrypt_with_block(schedule, field, 0)?;
            field_start = field_end;
        }
        // The picture data: the rest of the record, one field.
        if let Some(data) = body.get_mut(field_start..) {
            rc4::decrypt_with_block(schedule, data, 0)?;
        }
        at = body_end;
        index += 1;
    }
    Ok(())
}

/// The fixed fields of a blip before its data, by [MS-ODRAW] §2.2.24-§2.2.31: each
/// type's two instances differ only in whether a second UID follows the first.
#[cfg(test)]
fn blip_fields(rec_type: u16, ver_instance: u16) -> Result<Vec<usize>, Error> {
    let rec_ver = ver_instance & 0x000F;
    let instance = ver_instance >> 4;
    if rec_type == RT_FBSE {
        return Err(Error::UnsupportedEncryptedContent(
            "a file-block entry (OfficeArtFBSE) in the Pictures stream",
        ));
    }
    let (pair, trailer) = match rec_type {
        0xF01A => ([0x3D4, 0x3D4], METAFILE_HEADER_LEN), // EMF
        0xF01B => ([0x216, 0x216], METAFILE_HEADER_LEN), // WMF
        0xF01C => ([0x542, 0x542], METAFILE_HEADER_LEN), // PICT
        0xF01D => ([0x46A, 0x6E2], BITMAP_TAG_LEN),      // JPEG, RGB and CMYK
        0xF01E => ([0x6E0, 0x6E0], BITMAP_TAG_LEN),      // PNG
        0xF01F => ([0x7A8, 0x7A8], BITMAP_TAG_LEN),      // DIB
        0xF029 => ([0x6E4, 0x6E4], BITMAP_TAG_LEN),      // TIFF
        0xF018..=0xF117 => {
            return Err(Error::UnsupportedEncryptedContent(
                "a Pictures record of a blip type [MS-ODRAW] does not define",
            ))
        }
        // A record type is a value the file declares, so this is a bad parameter;
        // `UnsupportedEncryptedContent` is for content of a known kind not decrypted.
        other => {
            return Err(bad(
                FileField::PicturesRecord,
                FileProblem::NotInSet {
                    got: other.into(),
                    allowed: AllowedSet::PicturesRecordTypes,
                    bound: Bound::Spec,
                },
            ))
        }
    };
    if rec_ver != 0 || !pair.contains(&(instance & !1)) {
        return Err(bad(
            FileField::PicturesRecord,
            FileProblem::Unrecognized {
                got: ver_instance.into(),
            },
        ));
    }
    let mut fields = vec![UID_LEN];
    if instance & 1 == 1 {
        fields.push(UID_LEN);
    }
    fields.push(trailer);
    Ok(fields)
}
