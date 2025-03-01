// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::BufRead;
use std::io::Cursor;

use bstr::ByteSlice;
use common_arrow::arrow::bitmap::MutableBitmap;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::serialize::read_decimal_with_size;
use common_expression::serialize::uniform_date;
use common_expression::types::array::ArrayColumnBuilder;
use common_expression::types::date::check_date;
use common_expression::types::decimal::Decimal;
use common_expression::types::decimal::DecimalColumnBuilder;
use common_expression::types::decimal::DecimalSize;
use common_expression::types::nullable::NullableColumnBuilder;
use common_expression::types::number::Number;
use common_expression::types::string::StringColumnBuilder;
use common_expression::types::timestamp::check_timestamp;
use common_expression::types::AnyType;
use common_expression::types::NumberColumnBuilder;
use common_expression::with_decimal_type;
use common_expression::with_number_mapped_type;
use common_expression::ColumnBuilder;
use common_io::cursor_ext::BufferReadDateTimeExt;
use common_io::cursor_ext::DateTimeResType;
use common_io::cursor_ext::ReadBytesExt;
use common_io::cursor_ext::ReadCheckPointExt;
use common_io::cursor_ext::ReadNumberExt;
use jsonb::parse_value;
use lexical_core::FromLexical;

use crate::field_decoder::FieldDecoder;
use crate::CommonSettings;

pub trait FieldDecoderRowBased: FieldDecoder {
    fn common_settings(&self) -> &CommonSettings;

    fn ignore_field_end<R: AsRef<[u8]>>(&self, reader: &mut Cursor<R>) -> bool;

    fn match_bytes<R: AsRef<[u8]>>(&self, reader: &mut Cursor<R>, bs: &[u8]) -> bool {
        let pos = reader.checkpoint();
        if reader.ignore_bytes(bs) && self.ignore_field_end(reader) {
            true
        } else {
            reader.rollback(pos);
            false
        }
    }

    fn read_field<R: AsRef<[u8]>>(
        &self,
        column: &mut ColumnBuilder,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()> {
        match column {
            ColumnBuilder::Null { len } => self.read_null(len, reader, raw),
            ColumnBuilder::Nullable(c) => self.read_nullable(c, reader, raw),
            ColumnBuilder::Boolean(c) => self.read_bool(c, reader, raw),
            ColumnBuilder::Number(c) => with_number_mapped_type!(|NUM_TYPE| match c {
                NumberColumnBuilder::NUM_TYPE(c) => {
                    if NUM_TYPE::FLOATING {
                        self.read_float(c, reader, raw)
                    } else {
                        self.read_int(c, reader, raw)
                    }
                }
            }),
            ColumnBuilder::Decimal(c) => with_decimal_type!(|DECIMAL_TYPE| match c {
                DecimalColumnBuilder::DECIMAL_TYPE(c, size) =>
                    self.read_decimal(c, *size, reader, raw),
            }),
            ColumnBuilder::Date(c) => self.read_date(c, reader, raw),
            ColumnBuilder::Timestamp(c) => self.read_timestamp(c, reader, raw),
            ColumnBuilder::String(c) => self.read_string(c, reader, raw),
            ColumnBuilder::Array(c) => self.read_array(c, reader, raw),
            ColumnBuilder::Map(c) => self.read_map(c, reader, raw),
            ColumnBuilder::Bitmap(c) => self.read_string(c, reader, raw),
            ColumnBuilder::Tuple(fields) => self.read_tuple(fields, reader, raw),
            ColumnBuilder::Variant(c) => self.read_variant(c, reader, raw),
            _ => unimplemented!(),
        }
    }

    fn read_bool<R: AsRef<[u8]>>(
        &self,
        column: &mut MutableBitmap,
        reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()> {
        if self.match_bytes(reader, &self.common_settings().true_bytes) {
            column.push(true);
            Ok(())
        } else if self.match_bytes(reader, &self.common_settings().false_bytes) {
            column.push(false);
            Ok(())
        } else {
            let err_msg = format!(
                "Incorrect boolean value, expect {} or {}",
                self.common_settings().true_bytes.to_str().unwrap(),
                self.common_settings().false_bytes.to_str().unwrap()
            );
            Err(ErrorCode::BadBytes(err_msg))
        }
    }

    fn read_null<R: AsRef<[u8]>>(
        &self,
        column_len: &mut usize,
        _reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()> {
        *column_len += 1;
        Ok(())
    }

    fn read_nullable<R: AsRef<[u8]>>(
        &self,
        column: &mut NullableColumnBuilder<AnyType>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()> {
        if reader.eof() {
            column.push_null();
        } else if self.match_bytes(reader, &self.common_settings().null_bytes)
            && self.ignore_field_end(reader)
        {
            column.push_null();
            return Ok(());
        } else {
            self.read_field(&mut column.builder, reader, raw)?;
            column.validity.push(true);
        }
        Ok(())
    }

    fn read_string_inner<R: AsRef<[u8]>>(
        &self,
        reader: &mut Cursor<R>,
        out_buf: &mut Vec<u8>,
        raw: bool,
    ) -> Result<()>;

    fn read_int<T, R: AsRef<[u8]>>(
        &self,
        column: &mut Vec<T>,
        reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()>
    where
        T: Number + From<T::Native>,
        T::Native: FromLexical,
    {
        let v: T::Native = reader.read_int_text()?;
        column.push(v.into());
        Ok(())
    }

    fn read_float<T, R: AsRef<[u8]>>(
        &self,
        column: &mut Vec<T>,
        reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()>
    where
        T: Number + From<T::Native>,
        T::Native: FromLexical,
    {
        let v: T::Native = reader.read_float_text()?;
        column.push(v.into());
        Ok(())
    }

    fn read_decimal<R: AsRef<[u8]>, D: Decimal>(
        &self,
        column: &mut Vec<D>,
        size: DecimalSize,
        reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()> {
        let buf = reader.remaining_slice();
        let (n, n_read) = read_decimal_with_size(buf, size, false)?;
        column.push(n);
        reader.consume(n_read);
        Ok(())
    }

    fn read_string<R: AsRef<[u8]>>(
        &self,
        column: &mut StringColumnBuilder,
        reader: &mut Cursor<R>,
        _raw: bool,
    ) -> Result<()>;

    fn read_date<R: AsRef<[u8]>>(
        &self,
        column: &mut Vec<i32>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()> {
        let mut buf = Vec::new();
        self.read_string_inner(reader, &mut buf, raw)?;
        let mut buffer_readr = Cursor::new(&buf);
        let date = buffer_readr.read_date_text(&self.common_settings().timezone)?;
        let days = uniform_date(date);
        check_date(days as i64)?;
        column.push(days);
        Ok(())
    }

    fn read_timestamp<R: AsRef<[u8]>>(
        &self,
        column: &mut Vec<i64>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()> {
        let mut buf = Vec::new();
        self.read_string_inner(reader, &mut buf, raw)?;
        let mut buffer_readr = Cursor::new(&buf);
        let ts = if !buf.contains(&b'-') {
            buffer_readr.read_num_text_exact()?
        } else {
            let t = buffer_readr.read_timestamp_text(&self.common_settings().timezone, false)?;
            match t {
                DateTimeResType::Datetime(t) => {
                    if !buffer_readr.eof() {
                        let data = buf.to_str().unwrap_or("not utf8");
                        let msg = format!(
                            "fail to deserialize timestamp, unexpected end at pos {} of {}",
                            buffer_readr.position(),
                            data
                        );
                        return Err(ErrorCode::BadBytes(msg));
                    }
                    t.timestamp_micros()
                }
                _ => unreachable!(),
            }
        };
        check_timestamp(ts)?;
        column.push(ts);
        Ok(())
    }

    fn read_variant<R: AsRef<[u8]>>(
        &self,
        column: &mut StringColumnBuilder,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()> {
        let mut buf = Vec::new();
        self.read_string_inner(reader, &mut buf, raw)?;
        match parse_value(&buf) {
            Ok(value) => {
                value.write_to_vec(&mut column.data);
                column.commit_row();
            }
            Err(_) => {
                if self.common_settings().disable_variant_check {
                    column.put_slice(&buf);
                    column.commit_row();
                } else {
                    return Err(ErrorCode::BadBytes(format!(
                        "Invalid JSON value: {:?}",
                        String::from_utf8_lossy(&buf)
                    )));
                }
            }
        }
        Ok(())
    }

    fn read_array<R: AsRef<[u8]>>(
        &self,
        column: &mut ArrayColumnBuilder<AnyType>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()>;

    fn read_map<R: AsRef<[u8]>>(
        &self,
        column: &mut ArrayColumnBuilder<AnyType>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()>;

    fn read_tuple<R: AsRef<[u8]>>(
        &self,
        fields: &mut Vec<ColumnBuilder>,
        reader: &mut Cursor<R>,
        raw: bool,
    ) -> Result<()>;
}
