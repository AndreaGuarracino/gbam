use super::meta::{BlockMeta, FileInfo, FileMeta, FILE_INFO_SIZE};
use super::SIZE_LIMIT;
use crate::{u32_size, u64_size, u8_size, var_size_field_to_index, Fields, RawRecord, FIELDS_NUM};
use byteorder::{LittleEndian, WriteBytesExt};
use std::io::{Seek, SeekFrom, Write};
static GBAM_MAGIC: &[u8] = b"geeBAM10";

/// The data is held in blocks.
///
/// Fixed sized fields are written as fixed size blocks into file. All blocks
/// (for fixed size fields) except last one contain equal amount of data.
///
/// Variable sized fields are written as fixed size blocks. Blocks may contain
/// different amount of data. Variable sized fields are accompanied by separate
/// index in separate block for fixed size fields. Groups records before writing
/// out to file.
pub struct Writer<W>
where
    W: Write + Seek,
{
    chunks: Vec<Vec<u8>>,
    // Current item index
    offsets: [usize; FIELDS_NUM],
    num_items: [u32; FIELDS_NUM],
    file_meta: FileMeta,
    inner: W,
}
static mut num: u32 = 0;
impl<W> Writer<W>
where
    W: Write + Seek,
{
    pub fn new(mut inner: W) -> Self {
        // Make space for the FileInfo to be written into.
        inner
            .seek(SeekFrom::Start((FILE_INFO_SIZE) as u64))
            .unwrap();
        Writer {
            chunks: vec![vec![0; SIZE_LIMIT]; FIELDS_NUM],
            offsets: [0; FIELDS_NUM],
            num_items: [0; FIELDS_NUM],
            file_meta: FileMeta::new(),
            inner: inner,
        }
    }
    pub fn push_record(&mut self, record: &RawRecord) {
        let mut index_fields_buf: [u8; u32_size] = [0; u32_size];
        for field in Fields::iterator().filter(|f| {
            **f != Fields::SequenceLength
                && **f != Fields::TemplateLength // This fields are not written on their own. They hold index data for variable sized fields.
                && **f != Fields::RawTagsLen
                && **f != Fields::LName
                && **f != Fields::RawSeqLen
        }) {
            let new_data = record.get_bytes(field);
            match field {
                // Variable sized fields. Require update to index fields
                Fields::ReadName
                | Fields::RawQual
                | Fields::RawSequence
                | Fields::RawTags
                | Fields::RawCigar => {
                    // Write variable sized field
                    self.update_field_buf(field, new_data);
                    let offset = match self.offsets[*field as usize] {
                        // The buffer for this field has been flushed
                        0 => 0,
                        new_offset => new_offset - new_data.len(),
                    };
                    (&mut index_fields_buf[..])
                        .write_u32::<LittleEndian>(offset as u32)
                        .unwrap();
                    // Write fixed size index
                    self.update_field_buf(&var_size_field_to_index(field), &index_fields_buf);
                }
                // Fixed sized fields
                _ => {
                    self.update_field_buf(field, new_data);
                }
            }
        }
    }

    /// Used to write new data into buffers
    fn update_field_buf(&mut self, field: &Fields, new_data: &[u8]) {
        let mut offset_into_chunk = self.offsets[*field as usize];

        if offset_into_chunk + new_data.len() > SIZE_LIMIT {
            self.flush(field);
            offset_into_chunk = 0;
        }

        let cur_chunk = &mut self.chunks[*field as usize];
        let item_counter = &mut self.num_items[*field as usize];

        cur_chunk[offset_into_chunk..offset_into_chunk + new_data.len()].clone_from_slice(new_data);
        offset_into_chunk += new_data.len();
        self.offsets[*field as usize] = offset_into_chunk;
        *item_counter += 1;
    }

    fn flush(&mut self, field: &Fields) {
        let meta = self.generate_meta(field);
        let field_meta = self.file_meta.get_blocks(field);
        field_meta.push(meta);
        // Write the data
        self.inner
            .write(&self.chunks[*field as usize][0..self.offsets[*field as usize]])
            .unwrap();

        self.offsets[*field as usize] = 0;
        self.num_items[*field as usize] = 0;
    }

    fn generate_meta(&mut self, field: &Fields) -> BlockMeta {
        let seek_pos = self.inner.seek(SeekFrom::Current(0 as i64)).unwrap();
        BlockMeta {
            seekpos: seek_pos,
            numitems: self.num_items[*field as usize],
        }
    }

    /// Terminates the writer. Always call after writting all the data. Returns
    /// total amount of bytes written.
    pub fn finish(&mut self) -> std::io::Result<u64> {
        // DONT DELETE THIS! THIS HOW IT SUPPOSED TO WORK WHEN ALL FIELDS ARE AVAILABLE!
        // for field in self.fields_to_flush.iter_mut() {
        //     *field = true;
        // }
        /// ITER OVER GBAM FIELDS...
        // self.fields_to_flush[Fields::Mapq as usize] = true;
        // self.fields_to_flush[Fields::Pos as usize] = true;
        // self.flush();
        let meta_start_pos = self.inner.seek(SeekFrom::Current(0))?;
        // Write meta
        let main_meta = serde_json::to_string(&self.file_meta).unwrap();
        self.inner.write(&main_meta.as_bytes()[..])?;

        let total_bytes_written = self.inner.seek(SeekFrom::Current(0))?;
        // Revert back to the beginning of the file
        self.inner.seek(SeekFrom::Start(0)).unwrap();
        let file_meta = FileInfo::new([1, 0], meta_start_pos);
        self.inner.write(&Into::<Vec<u8>>::into(file_meta)[..])?;
        Ok(total_bytes_written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::single_thread::reader::*;
    use byteorder::ReadBytesExt;
    use std::io::Cursor;
    #[test]
    fn test_writer() {
        let raw_records = vec![RawRecord::default(); 2];
        let mut buf: Vec<u8> = vec![0; SIZE_LIMIT];
        let out = Cursor::new(&mut buf[..]);
        let mut writer = Writer::new(out);
        for rec in raw_records.iter() {
            writer.push_record(rec);
        }
        let total_bytes_written = writer.finish().unwrap();
        buf.resize(total_bytes_written as usize, 0);

        let in_cursor = Box::new(Cursor::new(buf));
        let mut parsing_template = ParsingTemplate::new();
        parsing_template.set_all();
        let mut reader = Reader::new(in_cursor, parsing_template).unwrap();
        let mut it = raw_records.iter();
        while let Some(rec) = reader.next() {
            let rec_orig = it.next().unwrap();
            let orig_map_q = rec_orig.get_bytes(&Fields::Mapq)[0];
            let orig_pos = rec_orig
                .get_bytes(&Fields::Pos)
                .read_u32::<LittleEndian>()
                .unwrap();
            assert_eq!(rec.pos.unwrap(), orig_pos);
            assert_eq!(rec.mapq.unwrap(), orig_map_q);
        }
    }
}
