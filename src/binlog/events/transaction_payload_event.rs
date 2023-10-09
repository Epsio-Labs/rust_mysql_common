// Copyright (c) 2023 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use crate::io::ReadMysqlExt;
use std::{borrow::Cow, cmp::min, convert::TryFrom, io};

use saturating::Saturating as S;

use super::BinlogEventHeader;
use crate::{
    binlog::{
        consts::{
            BinlogVersion, EventType, TransactionPayloadCompressionType, TransactionPayloadFields,
        },
        BinlogCtx, BinlogEvent, BinlogStruct,
    },
    io::{BufMutExt, ParseBuf},
    misc::raw::{bytes::EofBytes, int::*, RawBytes},
    proto::{MyDeserialize, MySerialize},
};

/// The rotate event is added to the binlog as last event
/// to tell the reader what binlog to request next.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TransactionPayloadEvent<'a> {
    // payload size
    payload_size: RawInt<LeU64>,

    // compression algorithm
    algorithm: TransactionPayloadCompressionType,

    // uncompressed size
    uncompressed_size: RawInt<LeU64>,

    // payload to be decompressed
    payload: RawBytes<'a, EofBytes>,

    // size of parsed header
    header_size: usize,
}

impl<'a> TransactionPayloadEvent<'a> {
    pub fn new(
        payload_size: u64,
        algorithm: TransactionPayloadCompressionType,
        uncompressed_size: u64,
        payload: impl Into<Cow<'a, [u8]>>,
    ) -> Self {
        Self {
            payload_size: RawInt::new(payload_size),
            algorithm: algorithm,
            uncompressed_size: RawInt::new(uncompressed_size),
            payload: RawBytes::new(payload),
            header_size: 0,
        }
    }

    /// Sets the `payload_size` field value.
    pub fn with_payload_size(mut self, payload_size: u64) -> Self {
        self.payload_size = RawInt::new(payload_size);
        self
    }
    /// Sets the `algorithm` field value.
    pub fn with_algorithm(mut self, algorithm: TransactionPayloadCompressionType) -> Self {
        self.algorithm = algorithm;
        self
    }
    /// Sets the `uncompressed_size` field value.
    pub fn with_uncompressed_size(mut self, uncompressed_size: u64) -> Self {
        self.uncompressed_size = RawInt::new(uncompressed_size);
        self
    }

    /// Sets the `payload` field value.
    pub fn with_payload(mut self, payload: impl Into<Cow<'a, [u8]>>) -> Self {
        self.payload = RawBytes::new(payload);
        self
    }

    /// Returns the payload_size.
    pub fn payload_size(&self) -> u64 {
        self.payload_size.0
    }

    /// Returns raw payload of the binlog event.
    pub fn payload_raw(&'a self) -> &'a [u8] {
        self.payload.as_bytes()
    }

    /// Returns raw payload decompressed (see [`crate::binlog::EventStreamReader::read_decompressed`]).
    pub fn decompress_payload(self) -> Vec<u8> {
        if self.algorithm == TransactionPayloadCompressionType::NONE {
            return self.payload_raw().to_vec();
        }
        let mut decode_buf = vec![0_u8; self.uncompressed_size.0 as usize];
        match zstd::stream::copy_decode(self.payload.as_bytes(), &mut decode_buf[..]) {
            Ok(_) => {}
            Err(_) => {
                return Vec::new();
            }
        };
        decode_buf
    }

    /// Returns the algorithm.
    pub fn algorithm(&self) -> TransactionPayloadCompressionType {
        self.algorithm
    }

    /// Returns the uncompressed_size.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size.0
    }

    pub fn into_owned(self) -> TransactionPayloadEvent<'static> {
        TransactionPayloadEvent {
            payload_size: self.payload_size,
            algorithm: self.algorithm,
            uncompressed_size: self.uncompressed_size,
            payload: self.payload.into_owned(),
            header_size: self.header_size,
        }
    }
}

impl<'de> MyDeserialize<'de> for TransactionPayloadEvent<'de> {
    const SIZE: Option<usize> = None;
    type Ctx = BinlogCtx<'de>;
    fn deserialize(_ctx: Self::Ctx, buf: &mut ParseBuf<'de>) -> io::Result<Self> {
        let mut ob = Self {
            payload_size: RawInt::new(0),
            algorithm: TransactionPayloadCompressionType::NONE,
            uncompressed_size: RawInt::new(0),
            payload: RawBytes::from("".as_bytes()),
            header_size: 0,
        };
        let mut have_payload_size = false;
        let mut have_compression_type = false;
        let original_buf_size = buf.len();
        while !buf.is_empty() {
            /* read the type of the field. */
            let field_type = buf.read_lenenc_int()?;
            match TransactionPayloadFields::try_from(field_type) {
                // we have reached the end of the header
                Ok(TransactionPayloadFields::OTW_PAYLOAD_HEADER_END_MARK) => {
                    if !have_payload_size || !have_compression_type {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("Missing field in payload header"),
                        ))?;
                    }
                    if ob.payload_size.0 as usize > buf.len() {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "Payload size is bigger than the remaining buffer: {} > {}",
                                ob.payload_size.0,
                                buf.len()
                            ),
                        ))?;
                    }
                    ob.header_size = original_buf_size - ob.payload_size.0 as usize;
                    let mut payload_buf: ParseBuf = buf.parse(ob.payload_size.0 as usize)?;
                    ob.payload = RawBytes::from(payload_buf.eat_all());
                    break;
                }

                Ok(TransactionPayloadFields::OTW_PAYLOAD_SIZE_FIELD) => {
                    let _length = buf.read_lenenc_int()?;
                    let val = buf.read_lenenc_int()?;
                    ob.payload_size = RawInt::new(val);
                    have_payload_size = true;
                    continue;
                }
                Ok(TransactionPayloadFields::OTW_PAYLOAD_COMPRESSION_TYPE_FIELD) => {
                    let _length = buf.read_lenenc_int()?;
                    let val = buf.read_lenenc_int()?;
                    ob.algorithm = TransactionPayloadCompressionType::try_from(val).unwrap();
                    have_compression_type = true;
                    continue;
                }
                Ok(TransactionPayloadFields::OTW_PAYLOAD_UNCOMPRESSED_SIZE_FIELD) => {
                    let _length = buf.read_lenenc_int()?;
                    let val = buf.read_lenenc_int()?;
                    ob.uncompressed_size = RawInt::new(val);
                    continue;
                }
                Err(_) => {
                    let length = buf.eat_lenenc_int();
                    buf.skip(length as usize);
                    continue;
                }
            };
        }

        Ok(ob)
    }
}

impl MySerialize for TransactionPayloadEvent<'_> {
    fn serialize(&self, buf: &mut Vec<u8>) {
        buf.put_lenenc_int(TransactionPayloadFields::OTW_PAYLOAD_COMPRESSION_TYPE_FIELD as u64);
        buf.put_lenenc_int(crate::misc::lenenc_int_len(self.algorithm as u64) as u64);
        buf.put_lenenc_int(self.algorithm as u64);

        if self.algorithm != TransactionPayloadCompressionType::NONE {
            buf.put_lenenc_int(
                TransactionPayloadFields::OTW_PAYLOAD_UNCOMPRESSED_SIZE_FIELD as u64,
            );
            buf.put_lenenc_int(crate::misc::lenenc_int_len(self.uncompressed_size.0) as u64);
            buf.put_lenenc_int(self.uncompressed_size.0);
        }

        buf.put_lenenc_int(TransactionPayloadFields::OTW_PAYLOAD_SIZE_FIELD as u64);
        buf.put_lenenc_int(crate::misc::lenenc_int_len(self.payload_size.0) as u64);
        buf.put_lenenc_int(self.payload_size.0);

        buf.put_lenenc_int(TransactionPayloadFields::OTW_PAYLOAD_HEADER_END_MARK as u64);

        self.payload.serialize(&mut *buf);
    }
}

impl<'a> BinlogEvent<'a> for TransactionPayloadEvent<'a> {
    const EVENT_TYPE: EventType = EventType::TRANSACTION_PAYLOAD_EVENT;
}

impl<'a> BinlogStruct<'a> for TransactionPayloadEvent<'a> {
    fn len(&self, _version: BinlogVersion) -> usize {
        let mut len = S(self.header_size);

        len += S(self.payload.0.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::LEN)
    }
}