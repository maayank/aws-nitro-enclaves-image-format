// Copyright 2019-2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::defs::eif_hasher::EifHasher;
use crate::defs::{
    EifHeader, EifIdentityInfo, EifSectionType, EifSectionHeader, EifSection, PcrInfo,
    PcrSignature,
};
use aws_nitro_enclaves_cose::{crypto::Openssl, CoseSign1};
use crc::{Crc, CRC_32_ISO_HDLC};
use openssl::pkey::PKey;
use serde::{Deserialize, Serialize};
use serde_cbor::{from_slice, to_vec};
use sha2::{Digest, Sha384};

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;

/// The information about the signing certificate to be provided for a `describe-eif` request.
#[derive(Clone, Serialize, Deserialize)]
pub struct SignCertificateInfo {
    #[serde(rename = "IssuerName")]
    /// Certificate's subject name.
    pub issuer_name: BTreeMap<String, String>,
    #[serde(rename = "Algorithm")]
    /// Certificate's signature algorithm
    pub algorithm: String,
    #[serde(rename = "NotBefore")]
    /// Not before validity period
    pub not_before: String,
    #[serde(rename = "NotAfter")]
    /// Not after validity period
    pub not_after: String,
    #[serde(rename = "Signature")]
    /// Certificate's signature in hex format: 'XX:XX:XX..'
    pub signature: String,
}

impl SignCertificateInfo {
    /// Create new signing certificate information structure
    pub fn new(
        issuer_name: BTreeMap<String, String>,
        algorithm: String,
        not_before: String,
        not_after: String,
        signature: String,
    ) -> Self {
        SignCertificateInfo {
            issuer_name,
            algorithm,
            not_before,
            not_after,
            signature,
        }
    }
}

/// Used to parse the EIF file into discrete
/// sections and their associated buffers
pub struct Sections {
    eif_file: File,
    header: EifHeader,
    curr_section: usize,
}

impl Sections {
    pub fn new(mut eif_file: File) -> Result<Self, String> {
        eif_file
            .rewind()
            .map_err(|e| format!("Failed to rewind EIF file: {:?}", e))?;

        let mut header_buf = vec![0u8; EifHeader::size()];
        eif_file
            .read_exact(&mut header_buf)
            .map_err(|e| format!("Error while reading EIF header: {:?}", e))?;

        let header = EifHeader::from_be_bytes(&header_buf)
            .map_err(|e| format!("Error while parsing EIF header: {:?}", e))?;

        Ok(Sections {
            eif_file,
            header,
            curr_section: 0,
        })
    }
}

impl Iterator for Sections {
    type Item = Result<EifSection, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr_section >= self.header.num_sections.into() {
            return None;
        }

        let section_offset = self.header.section_offsets[self.curr_section];
        let section_size = self.header.section_sizes[self.curr_section] as usize;
        self.curr_section += 1;

        let mut section_buf = vec![0u8; EifSectionHeader::size()];
        if self.eif_file.seek(SeekFrom::Start(section_offset)).is_err() {
            return Some(Err("Failed to seek to section offset".to_string()));
        }
        if self.eif_file.read_exact(&mut section_buf).is_err() {
            return Some(Err("Error while reading EIF section header".to_string()));
        }

        let header = match EifSectionHeader::from_be_bytes(&section_buf) {
            Ok(sec) => sec,
            Err(e) => return Some(Err(format!("Error extracting EIF section header: {:?}", e))),
        };

        let mut data = vec![0u8; section_size];
        if self.eif_file.seek(SeekFrom::Start(section_offset + EifSectionHeader::size() as u64)).is_err() {
            return Some(Err("Failed to seek after EIF header".to_string()));
        }
        if self.eif_file.read_exact(&mut data).is_err() {
            return Some(Err("Error while reading section from EIF".to_string()));
        }

        Some(Ok(EifSection {
            header,
            data,
        }))
    }
}

/// Used for providing EIF info when requested by
/// 'describe-eif' or 'describe-enclaves' commands
pub struct EifReader {
    /// Deserialized EIF header
    pub header: EifHeader,
    /// Serialized signature section
    pub signature_section: Option<Vec<u8>>,
    /// Hash of the whole EifImage.
    pub image_hasher: EifHasher<Sha384>,
    /// Hash of the EifSections provided by Amazon
    /// Kernel + cmdline + First Ramdisk
    pub bootstrap_hasher: EifHasher<Sha384>,
    /// Hash of the remaining ramdisks.
    pub app_hasher: EifHasher<Sha384>,
    /// Hash the signing certificate
    pub cert_hasher: EifHasher<Sha384>,
    pub eif_crc: u32,
    pub sign_check: Option<bool>,
    /// Generated and custom EIF metadata
    pub metadata: Option<EifIdentityInfo>,
}

impl EifReader {
    /// Reads EIF and extracts sections to be written in the hashers based
    /// on section type. Also writes sections in the eif_crc, excluding the
    /// CRC from the header
    pub fn from_eif(eif_path: String) -> Result<Self, String> {
        let crc_gen = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        let mut eif_crc = crc_gen.digest();
        let mut eif_file =
            File::open(eif_path).map_err(|e| format!("Failed to open the EIF file: {:?}", e))?;

        // Extract EIF header
        let mut header_buf = vec![0u8; EifHeader::size()];
        eif_file
            .read_exact(&mut header_buf)
            .map_err(|e| format!("Error while reading EIF header: {:?}", e))?;

        // Exclude last field of header which is CRC
        let len_without_crc = header_buf.len() - size_of::<u32>();
        eif_crc.update(&header_buf[..len_without_crc]);

        let header = EifHeader::from_be_bytes(&header_buf)
            .map_err(|e| format!("Error while parsing EIF header: {:?}", e))?;

        let sections = Sections::new(eif_file)?;
        let mut image_hasher = EifHasher::new_without_cache(Sha384::new())
            .map_err(|e| format!("Could not create image_hasher: {:?}", e))?;
        let mut bootstrap_hasher = EifHasher::new_without_cache(Sha384::new())
            .map_err(|e| format!("Could not create bootstrap_hasher: {:?}", e))?;
        let mut app_hasher = EifHasher::new_without_cache(Sha384::new())
            .map_err(|e| format!("Could not create app_hasher: {:?}", e))?;
        let mut cert_hasher = EifHasher::new_without_cache(Sha384::new())
            .map_err(|e| format!("Could not create cert_hasher: {:?}", e))?;
        let mut ramdisk_idx = 0;
        let mut signature_section = None;
        let mut metadata = None;

        // Read all sections and treat by type
        for section in sections {
            let section = section.map_err(|e| e.to_string())?;
            eif_crc.update(&section.header.to_be_bytes());
            eif_crc.update(&section.data);

            match section.header.section_type {
                EifSectionType::EifSectionKernel | EifSectionType::EifSectionCmdline => {
                    image_hasher.write_all(&section.data).map_err(|e| {
                        format!("Failed to write EIF section to image_hasher: {:?}", e)
                    })?;
                    bootstrap_hasher.write_all(&section.data).map_err(|e| {
                        format!("Failed to write EIF section to bootstrap_hasher: {:?}", e)
                    })?;
                }
                EifSectionType::EifSectionRamdisk => {
                    image_hasher.write_all(&section.data).map_err(|e| {
                        format!("Failed to write ramdisk section to image_hasher: {:?}", e)
                    })?;
                    if ramdisk_idx == 0 {
                        bootstrap_hasher.write_all(&section.data).map_err(|e| {
                            format!(
                                "Failed to write ramdisk section to bootstrap_hasher: {:?}",
                                e
                            )
                        })?;
                    } else {
                        app_hasher.write_all(&section.data).map_err(|e| {
                            format!("Failed to write ramdisk section to app_hasher: {:?}", e)
                        })?;
                    }
                    ramdisk_idx += 1;
                }
                EifSectionType::EifSectionSignature => {
                    signature_section = Some(section.data.clone());
                    // Deserialize PCR0 signature structure and write it to the hasher
                    let des_sign: Vec<PcrSignature> = from_slice(&section.data[..])
                        .map_err(|e| format!("Error deserializing certificate: {:?}", e))?;

                    let cert = openssl::x509::X509::from_pem(&des_sign[0].signing_certificate)
                        .map_err(|e| format!("Error while digesting certificate: {:?}", e))?;
                    let cert_der = cert.to_der().map_err(|e| {
                        format!("Failed to deserialize signing certificate: {:?}", e)
                    })?;
                    cert_hasher.write_all(&cert_der).map_err(|e| {
                        format!("Failed to write signature section to cert_hasher: {:?}", e)
                    })?;
                }
                EifSectionType::EifSectionMetadata => {
                    metadata = serde_json::from_slice(&section.data[..])
                        .map_err(|e| format!("Error deserializing metadata: {:?}", e))?;
                }
                EifSectionType::EifSectionInvalid => {
                    return Err("Found invalid EIF section".to_string());
                }
            }
        }

        Ok(EifReader {
            header,
            signature_section,
            image_hasher,
            bootstrap_hasher,
            app_hasher,
            cert_hasher,
            eif_crc: eif_crc.finalize(),
            sign_check: None,
            metadata,
        })
    }

    pub fn get_metadata(&self) -> Option<EifIdentityInfo> {
        self.metadata.clone()
    }

    /// Returns deserialized header section
    pub fn get_header(&self) -> EifHeader {
        self.header
    }

    /// Compare header CRC to the one we computed
    pub fn check_crc(&self) -> bool {
        self.header.eif_crc32 == self.eif_crc
    }

    /// Extract signature section from EIF and parse the signing certificate
    pub fn get_certificate_info(
        &mut self,
        measurements: BTreeMap<String, String>,
    ) -> Result<SignCertificateInfo, String> {
        let signature_buf = match &self.signature_section {
            Some(section) => section,
            None => {
                return Err("Signature section missing from EIF.".to_string());
            }
        };
        // Deserialize PCR0 signature structure and write it to the hasher
        let des_sign: Vec<PcrSignature> = from_slice(&signature_buf[..])
            .map_err(|e| format!("Error deserializing certificate: {:?}", e))?;

        let cert = openssl::x509::X509::from_pem(&des_sign[0].signing_certificate)
            .map_err(|e| format!("Error while digesting certificate: {:?}", e))?;

        // Parse issuer into a BTreeMap
        let mut issuer_name = BTreeMap::new();
        for e in cert.issuer_name().entries() {
            issuer_name.insert(
                e.object().to_string(),
                format!("{:?}", e.data()).replace(&['\"'][..], ""),
            );
        }

        let algorithm = format!("{:#?}", cert.signature_algorithm().object());

        // Get measured PCR0 signature payload
        let pcr0 = match measurements.get("PCR0") {
            Some(pcr) => pcr,
            None => {
                return Err("Failed to get PCR0.".to_string());
            }
        };
        let pcr_info = PcrInfo::new(
            0,
            hex::decode(pcr0).map_err(|e| format!("Error while decoding PCR0: {:?}", e))?,
        );

        let measured_payload =
            to_vec(&pcr_info).map_err(|e| format!("Could not serialize PCR info: {:?}", e))?;

        // Extract public key from certificate and convert to PKey
        let public_key = &cert
            .public_key()
            .map_err(|e| format!("Failed to get public key: {:?}", e))?;
        let coses_key = PKey::public_key_from_pem(
            &public_key
                .public_key_to_pem()
                .map_err(|e| format!("Failed to serialize public key: {:?}", e))?[..],
        )
        .map_err(|e| format!("Failed to decode key nit elliptic key structure: {:?}", e))?;

        // Deserialize COSE signature and extract the payload using the public key
        let pcr_sign = CoseSign1::from_bytes(&des_sign[0].signature[..])
            .map_err(|e| format!("Failed to deserialize signature: {:?}", e))?;
        let coses_payload = pcr_sign
            .get_payload::<Openssl>(Some(coses_key.as_ref()))
            .map_err(|e| format!("Failed to get signature payload: {:?}", e))?;

        self.sign_check = Some(measured_payload == coses_payload);

        Ok(SignCertificateInfo {
            issuer_name,
            algorithm,
            not_before: format!("{:#?}", cert.not_before()),
            not_after: format!("{:#?}", cert.not_after()),
            // Change format from [\  XX, \  X, ..] to XX:XX:XX...
            signature: format!("{:02X?}", cert.signature().as_slice().to_vec())
                .replace(&vec!['[', ']'][..], "")
                .replace(", ", ":"),
        })
    }
}
