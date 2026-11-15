#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Excel 97-2003 (`.xls`) decryption — [MS-XLS] §2.2.10, over XOR obfuscation
//! ([MS-OFFCRYPTO] §2.3.7) and the two RC4 families (§2.3.5, §2.3.6).
//!
//! What the format says (§2.2.10): the `Workbook` stream is a sequence of records
//! (§2.1.4: a 2-byte type, a 2-byte length, the body), and every record **body** is
//! obfuscated or encrypted except the record type and size, the bodies of `BOF`,
//! `FilePass`, `UsrExcl`, `FileLock`, `InterfaceHdr`, `RRDInfo` and `RRDHead`, and the
//! `lbPlyPos` field of `BoundSheet8`. For RC4, the stream is one keystream in 1024-byte
//! blocks numbered from zero at its start, and "for unencrypted records and the record
//! headers … a byte buffer of all zeros … is passed into the RC4 encryption function. The
//! results are then ignored" — so, as for Word, the stream is decrypted whole and the
//! clear bytes are put back. For XOR the array index for the byte at offset `k` of a
//! record body ending at stream offset `end` is `(end + k) % 16`; the spec leaves the
//! initial index to the application (§2.3.7.3 takes it as a parameter) and msoffcrypto
//! (`format/xls97.py`, `(data_index + count) % 16`) and LibreOffice
//! (`sc/source/filter/excel/xistream.cxx:189-193`, behaviour only) agree on that value.
//!
//! The `FILEPASS` record itself is left in place with its type zeroed and its body
//! zeroed — record `0x0000` of the same length, which Excel skips as unknown — because
//! removing it would move every `lbPlyPos` and that is what `msoffcrypto-tool -d` writes;
//! byte-identity with it is this slice's bar.
//!
//! Behaviour ported from msoffcrypto-tool `msoffcrypto/format/xls97.py` (MIT); see
//! NOTICE. office-crypto has no Excel path.

use crate::binary_office::{
    self, biff_records, BiffRecord, BIFF_BOF, BIFF_BOUNDSHEET8, BIFF_FILEPASS,
    BIFF_NEVER_ENCRYPTED, BOOK, WORKBOOK,
};
use crate::error::Error;
use crate::file_params::{bad, wide, AllowedSet, Bound, FileField, FileProblem};
use crate::legacy_container::LegacyContainer;
use crate::rc4::{self, BlockKeySchedule};
use crate::xor_obfuscation::XorObfuscator;
use crate::{rc4_cryptoapi, rc4_office97};

/// [MS-XLS] §2.2.10: 1024-byte blocks.
const BLOCK_SIZE: usize = 1024;
/// `BoundSheet8.lbPlyPos` — the 4 leading body bytes that stay in the clear (§2.4.28).
const LB_PLY_POS_LEN: usize = 4;
/// `FILEPASS.wEncryptionType` — §2.4.117.
const ENCRYPTION_TYPE_XOR: u16 = 0x0000;
const ENCRYPTION_TYPE_RC4: u16 = 0x0001;

enum Scheme {
    Xor(XorObfuscator),
    Rc4(Box<dyn BlockKeySchedule>),
}

/// Decrypt the container in place. The container is left untouched on any error.
pub(crate) fn decrypt(container: &mut LegacyContainer, password: &str) -> Result<(), Error> {
    let name = container
        .workbook_stream_name()
        .ok_or(Error::MissingStream("Workbook"))?;
    let book = container.read(name)?;

    // Walk every record once. A header that cannot be read, or a length past the
    // stream, is a malformed stream and is refused before anything is decrypted.
    //
    // **Nothing is collected.** This walk used to push every record into a
    // `Vec<BiffRecord>`, which is 24 bytes per record against a wire minimum of 4 — a
    // measured 9x peak-allocation amplification over the stream, reachable with a wrong
    // password, and unbounded because the record count is the file's to choose. At
    // `LEGACY_STREAM_READ_CAP` (1 GiB) that is ~9 GiB, which is an allocator abort no
    // caller can catch rather than an error any caller can handle. Two independent
    // reviewers measured it on 2026-09-05; see CHANGELOG.md.
    //
    // The pass below keeps one `BiffRecord` (the FILEPASS) and two `bool`s, so its
    // allocation is constant and the record count only costs time, which the stream cap
    // already bounds. Every record after FILEPASS is still validated here, before any
    // key is derived, so the error a malformed stream produces is unchanged.
    let mut filepass_record = None;
    let mut first_id = None;
    let mut out_of_order = None;
    for record in biff_records(&book) {
        let record = record.map_err(|e| {
            bad(
                FileField::BiffRecord,
                FileProblem::Unreadable { offset: wide(e.at) },
            )
        })?;
        if first_id.is_none() {
            first_id = Some(record.id);
        }
        // FILEPASS: [MS-XLS] §2.1.7.20 places it in the globals substream directly
        // after BOF, and §2.2.10 lets only the never-encrypted records precede it. One
        // found after a record that must be encrypted is neither an encrypted workbook
        // nor a plain one; it is refused by name rather than decrypted into garbage.
        if filepass_record.is_none() {
            if record.id == BIFF_FILEPASS {
                filepass_record = Some(record);
            } else if out_of_order.is_none() && !BIFF_NEVER_ENCRYPTED.contains(&record.id) {
                out_of_order = Some(record.id);
            }
        }
    }
    if first_id != Some(BIFF_BOF) {
        return Err(bad(FileField::WorkbookBof, FileProblem::Missing));
    }
    let Some(filepass_record) = filepass_record else {
        return Err(Error::NotEncrypted);
    };
    if let Some(out_of_order) = out_of_order {
        return Err(bad(
            FileField::Filepass,
            FileProblem::OutOfOrder {
                after_record: out_of_order,
            },
        ));
    }
    let filepass = &book[filepass_record.body.clone()];
    // BIFF5's FilePass omits the two-byte encryption type.
    let biff8 = binary_office::le16(&book, 4) == Some(0x0600);
    let scheme_type = if biff8 {
        binary_office::le16(filepass, 0)
    } else {
        Some(ENCRYPTION_TYPE_XOR)
    };
    let xor_offset = if biff8 { 2 } else { 0 };
    let scheme = match scheme_type {
        Some(ENCRYPTION_TYPE_XOR) => {
            // XORObfuscation: key(2) verificationBytes(2).
            let (Some(key), Some(verification_bytes)) = (
                binary_office::le16(filepass, xor_offset),
                binary_office::le16(filepass, xor_offset + 2),
            ) else {
                return Err(bad(
                    FileField::Filepass,
                    FileProblem::Truncated {
                        have: wide(filepass.len().saturating_sub(xor_offset)),
                        need: 4,
                    },
                ));
            };
            let obfuscator = XorObfuscator::new(password)?;
            obfuscator.verify(key, verification_bytes)?;
            Scheme::Xor(obfuscator)
        }
        Some(ENCRYPTION_TYPE_RC4) => {
            let structure = &filepass[2..];
            // §2.4.117: the first two bytes of the header structure choose the family.
            let schedule: Box<dyn BlockKeySchedule> = match (
                binary_office::le16(structure, 0),
                binary_office::le16(structure, 2),
            ) {
                (Some(1), Some(1)) => {
                    let header = rc4_office97::parse(structure)?;
                    let schedule = rc4_office97::Office97KeySchedule::new(password, &header.salt);
                    schedule.verify(&header)?;
                    Box::new(schedule)
                }
                (Some(2..=4), Some(2)) => {
                    let header = rc4_cryptoapi::parse(structure)?;
                    let schedule = rc4_cryptoapi::CryptoApiKeySchedule::new(
                        password,
                        &header.salt,
                        header.key_bits,
                    )?;
                    schedule.verify(&header)?;
                    Box::new(schedule)
                }
                (Some(major), Some(minor)) => {
                    return Err(Error::UnsupportedEncryptionVersion(major, minor))
                }
                _ => {
                    return Err(bad(
                        FileField::EncryptionVersionInfo,
                        FileProblem::Truncated {
                            have: wide(structure.len()),
                            need: 4,
                        },
                    ))
                }
            };
            Scheme::Rc4(schedule)
        }
        Some(other) => {
            return Err(bad(
                FileField::FilepassEncryptionType,
                FileProblem::NotInSet {
                    got: other.into(),
                    allowed: AllowedSet::XorOrRc4,
                    bound: Bound::Spec,
                },
            ))
        }
        None => {
            return Err(bad(
                FileField::Filepass,
                FileProblem::Truncated {
                    have: wide(filepass.len()),
                    need: 2,
                },
            ))
        }
    };

    let mut plain = book.clone();
    match &scheme {
        Scheme::Rc4(schedule) => {
            // One keystream over the whole stream; then every byte the format keeps in
            // the clear is restored from the original.
            rc4::decrypt_in_blocks(schedule.as_ref(), &mut plain, BLOCK_SIZE)?;
            // The second pass. The walk is re-run rather than remembered: every header
            // it yields was already validated above, so it cannot fail here, and
            // re-deriving them costs a linear scan instead of the 24 bytes of heap per
            // record. `flatten()` drops the error arm that the first pass proved empty.
            for record in biff_records(&book).flatten() {
                let header = record.body.start - 4..record.body.start;
                plain[header.clone()].copy_from_slice(&book[header]);
                let clear = clear_prefix(&record);
                let range = record.body.start..record.body.start + clear;
                plain[range.clone()].copy_from_slice(&book[range]);
            }
        }
        Scheme::Xor(obfuscator) => {
            // Per record, from the index the record's own end selects; the clear prefix
            // is skipped but still counts towards the index, as the spec's index over
            // the whole body implies and the oracle's output confirms.
            for record in biff_records(&book).flatten() {
                let clear = clear_prefix(&record);
                let start = record.body.start + clear;
                obfuscator.decrypt(&mut plain[start..record.body.end], record.body.end + clear);
            }
        }
    }

    // FILEPASS becomes record 0x0000 of the same length with a zero body.
    let fp = &filepass_record;
    plain[fp.body.start - 4..fp.body.start - 2].fill(0);
    plain[fp.body.clone()].fill(0);

    if name == BOOK {
        container.replace(WORKBOOK, &plain)?;
        container.remove(BOOK)?;
    } else {
        container.overwrite(name, &plain)?;
    }
    Ok(())
}

/// How many leading body bytes of `record` the format keeps in the clear: all of them
/// for the never-encrypted set, `lbPlyPos` for `BoundSheet8`, none otherwise.
fn clear_prefix(record: &BiffRecord) -> usize {
    if BIFF_NEVER_ENCRYPTED.contains(&record.id) {
        record.body.len()
    } else if record.id == BIFF_BOUNDSHEET8 {
        LB_PLY_POS_LEN.min(record.body.len())
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    #[test]
    fn biff5_xor_filepass_without_type_decrypts() {
        let workbook = [
            0x09, 0x08, 4, 0, 0, 5, 0, 0, 0x2f, 0, 4, 0, 0xce, 0xa6, 0x27, 0x97, 0x0a, 0, 0, 0,
        ];
        let mut storage = Cursor::new(Vec::new());
        {
            let mut file = cfb::CompoundFile::create(&mut storage).unwrap();
            file.create_stream("/Book")
                .unwrap()
                .write_all(&workbook)
                .unwrap();
            file.flush().unwrap();
        }
        let encrypted = storage.into_inner();
        let decrypted = crate::decrypt_binary_office(&encrypted, "testpass").unwrap();
        let mut cfb = cfb::CompoundFile::open(Cursor::new(decrypted)).unwrap();
        let mut result = Vec::new();
        cfb.open_stream("/Workbook")
            .unwrap()
            .read_to_end(&mut result)
            .unwrap();
        assert_eq!(&result[8..16], &[0, 0, 4, 0, 0, 0, 0, 0]);
        assert!(matches!(
            crate::decrypt_binary_office(&encrypted, "wrong"),
            Err(crate::Error::WrongPassword)
        ));
    }
}
