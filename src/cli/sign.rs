// SPDX-License-Identifier: Apache-2.0

use crate::backend::sev::snp::launch::{IdAuth, IdBlock};
use crate::backend::sev::snp::sign::Signature as IdSignature;
use crate::backend::ByteSized;
use crate::backend::{Backend, SevSignature, Signatures, BACKENDS};
use crate::exec::EXECS;

use std::fmt::Debug;
use std::fs::File;
use std::io::prelude::*;
use std::io::{stdout, Read};
use std::mem::size_of;
use std::ops::Deref;
use std::process::ExitCode;

use crate::backend::sev::snp::sign::PublicKey;
use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use clap::Args;
use p384::ecdsa::signature::Signer as _;
use p384::ecdsa::SigningKey;
use p384::elliptic_curve::bigint::Encoding;
use p384::elliptic_curve::sec1::Coordinates;
use p384::pkcs8::DecodePrivateKey;
use p384::EncodedPoint;
use sgx::crypto::{rcrypto::*, *};
use sgx::signature::{Author, Body, Signature};

// SAFETY: Signature is a C struct with no UD states and pointers.
unsafe impl ByteSized for Signature {}
// SAFETY: Body is a C struct with no UD states and pointers.
unsafe impl ByteSized for Body {}

/// Sign the compiled-in keep payload with the given keys.
#[derive(Args, Debug)]
pub struct Options {
    /// Binary to sign, which would be loaded and run inside the keep
    #[clap(value_name = "BINARY")]
    pub binpath: Option<Utf8PathBuf>,

    /// SGX RSA private key in PEM form
    #[clap(long)]
    sgx_key: Utf8PathBuf,

    /// File path to write the signature
    #[clap(long)]
    out: Option<Utf8PathBuf>,
}

fn sign_sgx(body_bytes: &[u8], sgx_key: &RS256PrivateKey) -> Result<Vec<u8>> {
    let body = Body::from_bytes(body_bytes).ok_or_else(|| anyhow!("Invalid SGX input data"))?;

    // FIXME: do we need the date and sw defined value?
    let author = Author::new(0, 0);
    let sig = Signature::new(sgx_key, author, body).context("Failed to create RSA signature")?;

    Ok(sig.as_bytes().to_vec())
}

fn sign_sev(id_block_bytes: &[u8], sev_key: &SigningKey, signature: &[u8]) -> Result<SevSignature> {
    if id_block_bytes.len() != size_of::<IdBlock>() {
        bail!("Invalid length of SEV input data");
    }

    let mut id_auth = IdAuth {
        id_key_algo: 1,   // ECDSA P-384 with SHA-384 as per SEV-SNP firmware spec
        auth_key_algo: 1, // ECDSA P-384 with SHA-384 as per SEV-SNP firmware spec
        ..Default::default()
    };

    // Sign the ID block with the SEV signing key.
    let sig: p384::ecdsa::Signature = sev_key.sign(id_block_bytes);
    // The r and s values have to be in little-endian order.
    let r = sig.r().as_ref().to_canonical().to_le_bytes();
    let s = sig.s().as_ref().to_canonical().to_le_bytes();
    // and are zero extended to the size of the components in the IdAuth struct.
    id_auth.id_block_sig.component.r[..r.len()].copy_from_slice(&r);
    id_auth.id_block_sig.component.s[..s.len()].copy_from_slice(&s);

    // get the coordinates of the public key of the signing key.
    let verifying_key: EncodedPoint = sev_key.verifying_key().to_encoded_point(false);
    let (mut r, mut s) = match verifying_key.coordinates() {
        Coordinates::Uncompressed { x, y } => (x.to_vec(), y.to_vec()),
        _ => bail!("Invalid verifying key"),
    };
    // The r and s values have to be in little-endian order.
    r.reverse();
    s.reverse();
    // and are zero extended to the size of the components in the IdAuth struct.
    id_auth.id_key.component.r[..r.len()].copy_from_slice(&r);
    id_auth.id_key.component.s[..s.len()].copy_from_slice(&s);

    id_auth.id_key_sig = IdSignature::from_bytes(&signature[..size_of::<IdSignature>()])
        .context("Failed to parse signature")?;
    id_auth.author_key = PublicKey::from_bytes(&signature[size_of::<IdSignature>()..])
        .context("Failed to parse author public key")?;

    Ok(SevSignature {
        id_block: id_block_bytes.to_vec(),
        id_auth: id_auth.as_bytes().to_vec(),
    })
}

impl Options {
    fn load_sgx_key(&self) -> Result<RS256PrivateKey> {
        let mut sgx_key_file =
            File::open(&self.sgx_key).context("Failed to open SGX private key file")?;
        let mut buffer = String::new();
        sgx_key_file.read_to_string(&mut buffer)?;
        let sgx_key = RS256PrivateKey::from_pem(&buffer).context("Failed to import SGX key")?;
        Ok(sgx_key)
    }

    pub fn execute(self) -> anyhow::Result<ExitCode> {
        println!("Signing the binary with the given keys");
        use mmarinus::{perms, Map, Private};
        let binary = if let Some(ref path) = self.binpath {
            Some(Map::load(path, Private, perms::Read)?)
        } else {
            None
        };

        let mut signatures = Signatures::default();

        for backend in BACKENDS.deref().iter() {
            let backend: &dyn Backend = backend.deref();

            if backend.shim().is_empty() {
                continue;
            }

            let exec = if let Some(ref e) = binary {
                e.as_ref()
            } else if let Some(e) = EXECS.iter().find(|w| w.with_backend(backend)) {
                e.exec()
            } else {
                continue;
            };

            if exec.is_empty() {
                continue;
            }

            let blob = backend.hash(backend.shim().as_ref(), exec.as_ref())?;

            match backend.name() {
                "sgx" => {
                    println!("Signing with SGX key");
                    let signature = sign_sgx(&blob, &self.load_sgx_key()?)?;
                    signatures.sgx = signature;
                }
                _ => {
                    continue;
                }
            };
        }

        if let Some(path) = self.out {
            let mut file = File::create(path)?;
            file.write_all(serde_json::to_string(&signatures)?.as_bytes())?;
        } else {
            stdout().write_all(serde_json::to_string(&signatures)?.as_bytes())?;
        }
        Ok(ExitCode::SUCCESS)
    }
}

#[cfg(test)]
mod test {
    use crate::cli::key::sev::sign::sign_id_sev_key;
    use crate::cli::sign::{sign_sev, sign_sgx};
    use p384::ecdsa::SigningKey;
    use p384::pkcs8::DecodePrivateKey;
    use sgx::crypto::{rcrypto::*, *};

    const SGX_KEY: &str = include_str!("../../tests/data/sgx-test.key");
    const SEV_ID_KEY: &str = include_str!("../../tests/data/sev-id.key");
    const SEV_AUTHOR_KEY: &str = include_str!("../../tests/data/sev-author.key");

    const SEV_IN: [u8; 96] = [
        255, 165, 145, 93, 184, 17, 227, 134, 166, 124, 80, 99, 74, 210, 44, 73, 78, 253, 225, 255,
        236, 152, 189, 138, 194, 109, 162, 157, 70, 219, 81, 136, 79, 24, 70, 89, 190, 39, 116,
        121, 93, 236, 54, 214, 57, 223, 252, 236, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 3, 0, 0, 0,
        0, 0,
    ];

    const SGX_IN: [u8; 128] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0,
        0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0,
        124, 255, 195, 180, 246, 57, 219, 115, 59, 98, 240, 212, 175, 143, 166, 98, 40, 238, 160,
        47, 140, 230, 9, 180, 243, 246, 196, 110, 169, 159, 112, 127, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 234, 1, 0,
    ];

    const SGX_OUT: [u8; 1808] = [
        6, 0, 0, 0, 225, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 96,
        0, 0, 0, 96, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 203, 92, 15, 51, 216, 135, 234, 79, 227, 118, 255, 77, 67, 90,
        168, 68, 104, 189, 227, 47, 90, 94, 187, 147, 184, 230, 191, 100, 30, 110, 244, 180, 166,
        10, 33, 201, 69, 77, 140, 242, 106, 111, 142, 0, 9, 27, 208, 158, 229, 3, 193, 25, 214, 44,
        163, 75, 141, 203, 237, 171, 31, 243, 140, 85, 91, 132, 200, 242, 187, 185, 204, 97, 12,
        233, 183, 226, 192, 61, 85, 67, 254, 51, 20, 246, 80, 206, 249, 101, 63, 157, 170, 173,
        101, 118, 186, 62, 111, 146, 137, 178, 80, 199, 70, 2, 71, 200, 0, 231, 211, 220, 138, 118,
        231, 129, 209, 80, 174, 51, 27, 251, 189, 8, 232, 221, 191, 107, 57, 10, 226, 32, 179, 176,
        3, 61, 143, 190, 138, 0, 229, 80, 90, 172, 173, 251, 228, 11, 128, 233, 101, 82, 92, 228,
        80, 45, 181, 158, 141, 79, 41, 229, 6, 108, 157, 235, 3, 243, 43, 69, 219, 214, 49, 201,
        117, 117, 158, 36, 212, 69, 61, 217, 205, 223, 75, 105, 32, 219, 97, 234, 97, 92, 145, 202,
        213, 170, 13, 84, 97, 69, 88, 47, 166, 237, 62, 37, 149, 93, 160, 127, 218, 98, 240, 4,
        176, 215, 9, 120, 15, 199, 35, 82, 218, 230, 175, 205, 222, 233, 60, 81, 44, 245, 192, 155,
        24, 1, 71, 185, 95, 135, 137, 33, 139, 85, 168, 236, 98, 13, 240, 84, 73, 69, 125, 112, 10,
        203, 176, 138, 89, 208, 115, 124, 148, 78, 50, 146, 183, 0, 124, 231, 0, 178, 130, 98, 194,
        140, 215, 95, 190, 24, 167, 47, 42, 171, 69, 231, 20, 137, 34, 255, 176, 206, 192, 117,
        250, 100, 99, 120, 238, 54, 106, 247, 26, 130, 146, 2, 65, 99, 172, 242, 243, 40, 80, 175,
        65, 177, 110, 211, 218, 173, 24, 245, 143, 35, 115, 155, 236, 32, 18, 130, 58, 226, 241,
        17, 55, 202, 55, 95, 149, 77, 170, 116, 251, 33, 35, 233, 203, 173, 163, 200, 235, 3, 148,
        22, 50, 25, 209, 204, 9, 188, 114, 51, 144, 132, 146, 173, 12, 208, 130, 14, 197, 116, 201,
        223, 74, 69, 51, 188, 13, 230, 92, 252, 53, 122, 168, 207, 3, 0, 0, 0, 173, 149, 138, 232,
        89, 146, 229, 219, 43, 200, 96, 160, 80, 246, 140, 90, 96, 10, 119, 172, 255, 44, 211, 72,
        200, 123, 68, 38, 23, 175, 77, 23, 158, 26, 214, 9, 119, 53, 47, 159, 179, 96, 79, 58, 0,
        204, 204, 105, 183, 103, 192, 115, 158, 103, 98, 116, 14, 205, 253, 3, 229, 200, 85, 247,
        105, 16, 123, 59, 179, 122, 7, 217, 139, 229, 147, 63, 139, 31, 225, 165, 31, 138, 186,
        184, 125, 113, 127, 96, 193, 116, 12, 65, 5, 118, 129, 61, 238, 239, 185, 84, 65, 196, 182,
        165, 194, 161, 240, 64, 15, 92, 60, 198, 253, 170, 77, 185, 245, 90, 47, 23, 221, 32, 196,
        108, 200, 153, 142, 119, 83, 110, 4, 122, 244, 26, 6, 133, 40, 152, 91, 56, 93, 218, 182,
        106, 62, 0, 69, 9, 171, 166, 37, 128, 56, 132, 253, 91, 56, 197, 148, 93, 111, 232, 59, 49,
        221, 19, 145, 145, 126, 193, 44, 105, 53, 72, 74, 68, 1, 166, 138, 176, 165, 250, 186, 14,
        108, 157, 200, 167, 41, 114, 76, 127, 93, 181, 137, 159, 223, 246, 136, 57, 79, 97, 173,
        211, 228, 102, 21, 58, 220, 46, 162, 197, 201, 75, 125, 124, 7, 164, 228, 49, 100, 226, 54,
        63, 178, 124, 133, 149, 89, 149, 60, 36, 8, 47, 109, 15, 230, 186, 33, 26, 201, 246, 212,
        209, 74, 85, 202, 54, 123, 234, 252, 11, 97, 41, 112, 234, 243, 23, 24, 35, 36, 67, 52,
        226, 72, 144, 192, 181, 163, 204, 128, 23, 96, 110, 227, 252, 227, 13, 255, 50, 143, 62,
        57, 91, 164, 153, 126, 152, 251, 36, 146, 190, 233, 125, 121, 63, 226, 102, 233, 74, 127,
        71, 84, 94, 213, 57, 24, 104, 131, 86, 220, 151, 27, 255, 236, 145, 110, 5, 175, 177, 132,
        31, 122, 1, 67, 125, 187, 86, 223, 89, 48, 43, 123, 158, 24, 145, 246, 191, 229, 211, 242,
        89, 8, 25, 225, 255, 105, 30, 5, 36, 169, 152, 49, 24, 29, 96, 32, 29, 33, 54, 203, 229,
        201, 70, 175, 72, 199, 157, 255, 92, 10, 194, 226, 241, 190, 226, 44, 243, 140, 231, 17,
        216, 181, 153, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0,
        0, 0, 0, 0, 0, 124, 255, 195, 180, 246, 57, 219, 115, 59, 98, 240, 212, 175, 143, 166, 98,
        40, 238, 160, 47, 140, 230, 9, 180, 243, 246, 196, 110, 169, 159, 112, 127, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 234, 1,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 13, 169, 255, 84, 207, 45, 104, 126, 32, 51, 47, 22,
        24, 68, 12, 96, 251, 170, 210, 153, 194, 78, 144, 50, 139, 166, 162, 125, 206, 22, 154, 63,
        41, 243, 105, 131, 67, 180, 240, 138, 40, 161, 109, 252, 189, 26, 219, 5, 57, 166, 145, 8,
        139, 113, 149, 152, 154, 46, 186, 64, 228, 149, 32, 226, 177, 175, 91, 28, 7, 5, 228, 227,
        173, 149, 180, 61, 52, 119, 189, 36, 235, 38, 145, 31, 254, 98, 70, 2, 239, 181, 239, 232,
        13, 62, 218, 75, 228, 91, 124, 35, 234, 7, 132, 127, 255, 129, 157, 69, 162, 80, 245, 230,
        133, 193, 230, 235, 53, 160, 132, 23, 129, 109, 237, 4, 0, 61, 90, 55, 146, 225, 125, 48,
        175, 85, 43, 155, 41, 119, 15, 50, 161, 161, 81, 157, 237, 199, 170, 150, 215, 91, 91, 74,
        144, 196, 93, 133, 112, 135, 238, 88, 126, 127, 187, 60, 163, 101, 69, 183, 51, 233, 228,
        220, 150, 249, 127, 114, 234, 110, 131, 206, 241, 202, 239, 98, 61, 230, 35, 13, 183, 85,
        13, 218, 202, 88, 73, 37, 205, 5, 216, 24, 215, 214, 97, 49, 135, 249, 142, 195, 70, 121,
        49, 29, 182, 100, 49, 214, 0, 112, 249, 116, 12, 189, 203, 28, 7, 163, 119, 157, 56, 196,
        57, 134, 217, 173, 73, 98, 239, 208, 227, 238, 255, 75, 4, 222, 228, 139, 118, 224, 133,
        128, 164, 167, 11, 31, 184, 126, 154, 92, 92, 210, 197, 21, 116, 250, 51, 122, 170, 78,
        172, 12, 99, 86, 164, 117, 33, 64, 169, 177, 33, 150, 248, 65, 105, 39, 99, 86, 217, 119,
        23, 94, 142, 87, 99, 74, 236, 242, 88, 18, 105, 182, 77, 2, 64, 32, 122, 115, 47, 229, 23,
        125, 58, 60, 53, 199, 110, 27, 14, 172, 209, 34, 83, 79, 18, 50, 120, 147, 40, 8, 180, 7,
        207, 168, 235, 218, 115, 86, 83, 84, 237, 198, 76, 148, 151, 94, 219, 190, 63, 196, 44,
        221, 111, 16, 98, 86, 64, 1, 8, 216, 70, 83, 82, 136, 119, 120, 40, 20, 241, 238, 222, 159,
        249, 43, 98, 217, 184, 248, 192, 171, 184, 193, 19, 24, 199, 113, 254, 6, 170, 68, 168, 2,
        188, 163, 130, 241, 49, 55, 60, 85, 232, 46, 143, 91, 94, 172, 32, 142, 158, 66, 74, 174,
        73, 95, 71, 28, 106, 54, 32, 141, 81, 102, 43, 20, 99, 49, 88, 34, 253, 228, 66, 37, 24,
        62, 32, 211, 224, 166, 27, 29, 121, 10, 184, 221, 91, 245, 16, 109, 1, 194, 146, 243, 115,
        67, 229, 183, 233, 174, 191, 30, 246, 247, 15, 192, 58, 196, 130, 47, 60, 124, 56, 153,
        168, 198, 26, 169, 250, 133, 255, 111, 40, 179, 152, 187, 189, 178, 249, 178, 84, 105, 5,
        45, 226, 108, 59, 13, 246, 111, 239, 242, 54, 65, 54, 68, 9, 31, 144, 20, 179, 30, 209,
        255, 29, 94, 179, 2, 192, 142, 197, 74, 28, 142, 154, 127, 191, 230, 143, 252, 208, 84, 22,
        41, 182, 39, 21, 231, 53, 119, 212, 176, 87, 179, 143, 227, 46, 234, 3, 114, 43, 172, 205,
        121, 116, 98, 53, 5, 8, 174, 138, 32, 204, 231, 201, 124, 58, 177, 136, 45, 125, 149, 120,
        75, 67, 241, 78, 59, 7, 20, 69, 164, 126, 161, 60, 50, 182, 40, 16, 95, 111, 243, 34, 203,
        248, 22, 35, 49, 13, 58, 107, 139, 167, 223, 250, 164, 175, 17, 46, 57, 29, 151, 149, 250,
        245, 126, 65, 35, 73, 194, 188, 255, 202, 111, 217, 181, 63, 41, 248, 244, 34, 196, 167,
        60, 251, 118, 136, 252, 77, 190, 205, 88, 185, 153, 238, 149, 93, 116, 53, 192, 65, 79,
        219, 48, 77, 157, 116, 174, 122, 129, 189, 76, 195, 86, 110, 154, 188, 161, 142, 6, 218,
        243, 246, 76, 109, 166, 128, 156, 225, 181, 201, 25, 107, 25, 126, 182, 22, 42, 214, 142,
        216, 209, 54, 0, 48, 80, 131, 224, 222, 70, 141, 17, 136, 252, 142, 244, 53, 143, 159, 69,
        72, 105, 201, 84, 176, 213, 96, 44, 250, 69, 74, 227, 137, 7, 74, 136, 196, 12, 59, 62, 52,
        109, 212, 5, 79, 191, 62, 227, 48, 143, 49, 116, 93, 9, 255, 85, 124, 43, 64, 77, 1, 63,
        71, 225, 115, 38, 38, 100, 225, 67, 210, 143, 57, 75, 147, 157, 209, 105, 213, 117, 159,
        60,
    ];

    const SEV_OUT: [u8; 4096] = [
        1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 220, 128, 197, 81, 33, 60, 72, 4, 168, 201, 162, 99, 115, 181, 178, 2, 101, 5,
        51, 8, 168, 64, 97, 194, 11, 191, 187, 116, 175, 225, 172, 84, 185, 130, 44, 235, 224, 186,
        105, 191, 94, 215, 204, 149, 88, 36, 139, 134, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 81, 28, 232, 119, 5, 67, 248, 31, 211, 195, 239, 84, 214, 200,
        254, 59, 135, 49, 64, 189, 216, 95, 234, 177, 23, 55, 73, 37, 101, 25, 240, 53, 43, 168,
        88, 126, 95, 5, 221, 51, 59, 161, 244, 184, 136, 132, 50, 97, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 3, 206, 125,
        70, 54, 237, 25, 165, 255, 186, 199, 168, 106, 80, 228, 85, 21, 50, 159, 231, 150, 16, 197,
        85, 117, 140, 118, 26, 4, 214, 98, 75, 191, 35, 27, 244, 192, 24, 14, 169, 203, 175, 161,
        84, 184, 84, 163, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 1, 166, 215, 10, 186, 204, 30, 193, 179, 207, 228, 184, 9, 55, 13, 70, 209, 212, 4, 221,
        242, 203, 65, 188, 66, 213, 211, 56, 111, 145, 145, 103, 38, 120, 246, 220, 43, 165, 172,
        73, 105, 103, 39, 98, 249, 28, 77, 33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 241, 130, 101, 138, 166, 71, 88, 213,
        16, 19, 152, 20, 233, 229, 89, 12, 186, 158, 70, 23, 182, 27, 7, 204, 111, 220, 250, 37,
        172, 89, 114, 91, 178, 51, 77, 182, 172, 185, 170, 83, 10, 185, 250, 100, 205, 95, 235, 78,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 92, 238, 63, 2,
        214, 41, 187, 229, 106, 235, 22, 64, 192, 77, 22, 201, 128, 249, 62, 74, 6, 6, 229, 154,
        255, 205, 174, 176, 194, 36, 228, 125, 120, 0, 194, 189, 148, 197, 102, 182, 24, 38, 91,
        202, 77, 43, 71, 165, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 22, 103, 59, 205, 223, 246, 137, 57, 97, 206, 250,
        43, 174, 247, 255, 194, 13, 229, 22, 250, 64, 156, 185, 245, 244, 52, 248, 154, 242, 131,
        94, 97, 188, 39, 112, 90, 42, 0, 124, 100, 221, 97, 147, 156, 50, 210, 127, 27, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 81, 21, 94, 213, 126, 210, 4,
        42, 100, 151, 250, 254, 252, 138, 110, 84, 183, 180, 237, 250, 166, 135, 226, 42, 164, 101,
        77, 161, 86, 234, 93, 81, 211, 102, 34, 193, 202, 194, 104, 177, 61, 78, 162, 189, 153, 76,
        39, 183, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn test_sgx_vector() {
        let key = RS256PrivateKey::from_pem(SGX_KEY).unwrap();
        let out = sign_sgx(SGX_IN.as_slice(), &key).unwrap();
        assert_eq!(SGX_OUT.as_slice(), out.as_slice());
    }

    #[test]
    fn test_sev_vector() {
        let author_key = SigningKey::from_pkcs8_pem(SEV_AUTHOR_KEY).unwrap();
        let id_key = SigningKey::from_pkcs8_pem(SEV_ID_KEY).unwrap();
        let signature = sign_id_sev_key(&author_key, &id_key).unwrap();

        let out = sign_sev(SEV_IN.as_slice(), &id_key, &signature).unwrap();

        assert_eq!(SEV_IN.as_slice(), out.id_block.as_slice());
        assert_eq!(SEV_OUT.as_slice(), out.id_auth.as_slice());
    }
}
