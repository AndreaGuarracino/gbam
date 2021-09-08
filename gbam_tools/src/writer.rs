use super::meta::{BlockMeta, Codecs, FileInfo, FileMeta, FILE_INFO_SIZE};
use crate::compressor::{CompressTask, Compressor, OrderingKey};
use crate::stats::StatsCollector;
use crate::{SIZE_LIMIT, U32_SIZE};
use bam_tools::record::bamrawrecord::BAMRawRecord;
use bam_tools::record::fields::{
    field_type, is_data_field, var_size_field_to_index, FieldType, Fields, FIELDS_NUM,
};
use byteorder::{LittleEndian, WriteBytesExt};
use crc32fast::Hasher;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io::{Seek, SeekFrom, Write};

pub(crate) struct BlockInfo {
    pub numitems: u32,
    pub uncompr_size: usize,
    pub field: Fields,
    // Interpretation is up to the reader.
    pub max_value: Option<Vec<u8>>,
    pub min_value: Option<Vec<u8>>,
}

impl Default for BlockInfo {
    fn default() -> Self {
        Self {
            numitems: 0,
            uncompr_size: 0,
            field: Fields::RefID,
            max_value: None,
            min_value: None,
        }
    }
}

/// The data is held in blocks.
///
/// Fixed sized fields are written as fixed size blocks into file. All blocks
/// (for fixed size fields) except last one contain equal amount of data.
///
/// Variable sized fields are written as fixed size blocks. Blocks may contain
/// different amount of data. Variable sized fields are accompanied by separate
/// index in separate block for fixed size fields. Groups records before writing
/// out to file.
pub struct Writer<WS>
where
    WS: Write + Seek,
{
    file_meta: FileMeta,
    columns: Vec<Box<dyn Column>>,
    compressor: Compressor,
    inner: WS,
}

impl<WS> Writer<WS>
where
    WS: Write + Seek,
{
    pub fn new(
        mut inner: WS,
        codecs: Vec<Codecs>,
        thread_num: usize,
        mut comparators: HashMap<Fields, StatsComparator>,
        ref_seqs: Vec<(String, i32)>,
    ) -> Self {
        inner
            .seek(SeekFrom::Start((FILE_INFO_SIZE) as u64))
            .unwrap();

        let mut columns = Vec::new();

        let mut count = 0;
        for field in Fields::iterator().filter(|f| is_data_field(*f)) {
            let comparator = comparators.remove(field).and_then(|val| Some(val));
            let col = match field_type(field) {
                FieldType::FixedSized => {
                    Box::new(FixedColumn::new(*field, comparator)) as Box<dyn Column>
                }
                FieldType::VariableSized => {
                    count += 1;
                    Box::new(VariableColumn::new(*field, comparator)) as Box<dyn Column>
                }
            };
            columns.push(col);
            count += 1;
        }
        debug_assert!(count == FIELDS_NUM);

        Self {
            // TODO: Codecs (currently only one is supported).
            file_meta: FileMeta::new(codecs[0], ref_seqs),
            inner,
            compressor: Compressor::new(thread_num),
            columns,
        }
    }

    pub fn new_no_stats(
        inner: WS,
        codecs: Vec<Codecs>,
        thread_num: usize,
        ref_seqs: Vec<(String, i32)>,
    ) -> Self {
        Self::new(
            inner,
            codecs,
            thread_num,
            HashMap::<Fields, StatsComparator>::new(),
            ref_seqs,
        )
    }

    /// Push BAM record into this writer
    pub fn push_record(&mut self, record: &BAMRawRecord) {
        // Index fields are not written on their own. They hold index data for variable sized fields.
        for col in self.columns.iter_mut() {
            while let WriteStatus::Full(inner) = col.write_record_field(&record) {
                flush_field_buffer(
                    &mut self.inner,
                    &mut self.file_meta,
                    &mut self.compressor,
                    inner,
                );
            }
        }
    }

    /// Terminates the writer. Always call after writting all the data. Returns
    /// total amount of bytes written.
    pub fn finish(&mut self) -> std::io::Result<u64> {
        // Flush leftovers
        let mut columns: Vec<Box<dyn Column>> = self.columns.drain(..).collect();
        for (inner, idx) in columns.iter_mut().map(|col| col.get_inners()) {
            let writer = &mut self.inner;
            let meta = &mut self.file_meta;
            let compress = &mut self.compressor;

            flush_field_buffer(writer, meta, compress, inner);
            if let Some(idx_inner) = idx {
                flush_field_buffer(writer, meta, compress, idx_inner);
            }
        }

        for task in self.compressor.finish() {
            if let OrderingKey::Key(key) = task.ordering_key {
                write_data_and_update_meta(&mut self.inner, &mut self.file_meta, key, &task);
            }
        }

        let meta_start_pos = self.inner.seek(SeekFrom::Current(0))?;
        // Write meta
        let main_meta = serde_json::to_string(&self.file_meta).unwrap();
        let main_meta_bytes = main_meta.as_bytes();
        let crc32 = calc_crc_for_meta_bytes(main_meta_bytes);
        self.inner.write_all(main_meta_bytes)?;

        let total_bytes_written = self.inner.seek(SeekFrom::Current(0))?;
        // Revert back to the beginning of the file
        self.inner.seek(SeekFrom::Start(0)).unwrap();
        let file_meta = FileInfo::new([1, 0], meta_start_pos, crc32);
        let file_meta_bytes = &Into::<Vec<u8>>::into(file_meta)[..];
        self.inner.write_all(file_meta_bytes)?;
        Ok(total_bytes_written)
    }
}

fn flush_field_buffer<WS: Write + Seek>(
    writer: &mut WS,
    file_meta: &mut FileMeta,
    compressor: &mut Compressor,
    inner: &mut Inner,
) {
    let field = &inner.field;
    let completed_task = compressor.get_compr_block();

    if let OrderingKey::Key(key) = completed_task.ordering_key {
        write_data_and_update_meta(writer, file_meta, key, &completed_task);
    }

    let old_buffer = &mut inner.buffer;

    let data = std::mem::replace(old_buffer, completed_task.buf);

    let codec = *file_meta.get_field_codec(&field);

    compressor.compress_block(
        OrderingKey::Key(inner.block_num),
        inner.generate_block_info(),
        data,
        codec,
    );

    inner.reset_for_new_block();
}

fn write_data_and_update_meta<WS: Write + Seek>(
    writer: &mut WS,
    file_meta: &mut FileMeta,
    key: u32,
    task: &CompressTask,
) {
    let compressed_size = task.buf.len();
    let meta = generate_meta(
        writer,
        task.block_info.numitems,
        compressed_size.try_into().unwrap(),
    );

    writer.write_all(&task.buf).unwrap();

    let field_meta = file_meta.get_blocks(&task.block_info.field);
    if field_meta.len() <= key as usize {
        field_meta.resize(key as usize + 1, BlockMeta::default());
    }

    // Order as came in
    field_meta[key as usize] = meta;
}

fn generate_meta<S: Seek>(writer: &mut S, numitems: u32, block_size: u32) -> BlockMeta {
    let seekpos = writer.seek(SeekFrom::Current(0)).unwrap();
    BlockMeta {
        seekpos,
        numitems,
        block_size,
        max_value: None,
        min_value: None,
    }
}

enum WriteStatus<'a> {
    Written,
    // Column or its index is at capacity. Flush it.
    Full(&'a mut Inner),
}

struct Inner {
    stats_collector: Option<StatsCollector>,
    buffer: Vec<u8>,
    offset: usize,
    field: Fields,
    rec_count: u32,
    block_num: u32,
}

type StatsComparator = Box<dyn Fn(&[u8], &[u8]) -> Ordering>;

impl Inner {
    pub fn new(field: Fields, comparator: Option<StatsComparator>) -> Self {
        Self {
            stats_collector: comparator.and_then(|cmp| Some(StatsCollector::new(field, cmp))),
            buffer: Vec::new(),
            offset: 0,
            field,
            rec_count: 0,
            block_num: 0,
        }
    }
    pub fn write_data(&mut self, data: &[u8]) -> WriteStatus {
        // At this point everything should be flushed.
        debug_assert!(!self.flush_required(&data));

        if self.buffer.len() < SIZE_LIMIT {
            self.buffer.resize(std::cmp::max(data.len(), SIZE_LIMIT), 0);
        }

        self.buffer[self.offset..self.offset + data.len()].clone_from_slice(data);
        self.offset += data.len();

        self.rec_count += 1;

        WriteStatus::Written
    }

    pub fn flush_required(&self, data: &[u8]) -> bool {
        // At least one record will be written in even if it exceeds SIZE_LIMIT.
        self.offset > 0 && self.offset + data.len() > SIZE_LIMIT
    }

    pub fn reset_for_new_block(&mut self) {
        if let Some(ref mut stats) = self.stats_collector {
            stats.reset()
        };
        self.offset = 0;
        self.rec_count = 0;
        self.block_num += 1;
    }

    pub fn generate_block_info(&self) -> BlockInfo {
        BlockInfo {
            numitems: self.rec_count,
            uncompr_size: self.offset,
            field: self.field,
            max_value: self
                .stats_collector
                .as_ref()
                .and_then(|st| st.max_value.clone()),
            min_value: self
                .stats_collector
                .as_ref()
                .and_then(|st| st.min_value.clone()),
        }
    }
}

trait Column {
    // Extracts and writes data from corresponding BAMRawRecord record.
    fn write_record_field(&mut self, rec: &BAMRawRecord) -> WriteStatus;

    fn get_inners(&mut self) -> (&mut Inner, Option<&mut Inner>);
}

/// Column containing fixed sized fields.
struct FixedColumn(Inner);

impl FixedColumn {
    pub fn new(field: Fields, comparator: Option<StatsComparator>) -> Self {
        Self(Inner::new(field, comparator))
    }
}

impl Column for FixedColumn {
    fn write_record_field(&mut self, rec: &BAMRawRecord) -> WriteStatus {
        let inner = &mut self.0;
        let data = rec.get_bytes(&inner.field);

        if inner.flush_required(data) {
            return WriteStatus::Full(inner);
        }

        if let Some(ref mut stats) = inner.stats_collector {
            stats.update(data);
        }

        inner.write_data(data)
    }

    fn get_inners(&mut self) -> (&mut Inner, Option<&mut Inner>) {
        (&mut self.0, None)
    }
}

struct VariableColumn {
    inner: Inner,
    index: FixedColumn,
}

impl VariableColumn {
    pub fn new(field: Fields, comparator: Option<StatsComparator>) -> Self {
        Self {
            inner: Inner::new(field, comparator),
            index: FixedColumn::new(var_size_field_to_index(&field), None),
        }
    }
}

impl Column for VariableColumn {
    fn write_record_field(&mut self, rec: &BAMRawRecord) -> WriteStatus {
        let inner = &mut self.inner;
        let index_inner = &mut self.index.0;

        let data = rec.get_bytes(&inner.field);
        let mut idx_buf: [u8; U32_SIZE] = [0; U32_SIZE];

        if index_inner.flush_required(&idx_buf) {
            return WriteStatus::Full(index_inner);
        }

        if inner.flush_required(data) {
            return WriteStatus::Full(inner);
        }

        if let Some(ref mut stats) = inner.stats_collector {
            stats.update(data);
        }

        inner.write_data(data);
        (&mut idx_buf[..])
            .write_u32::<LittleEndian>(inner.offset as u32)
            .unwrap();
        index_inner.write_data(&idx_buf)
    }

    fn get_inners(&mut self) -> (&mut Inner, Option<&mut Inner>) {
        (&mut self.inner, Some(&mut self.index.0))
    }
}

impl<W> Write for Writer<W>
where
    W: Write + Seek,
{
    /// WARNING: ENSURE THAT BUF CONTAINS A ONE FULL RECORD.
    /// Write trait implementation is made to allow passing Write trait objects to sort function in BAM parallel.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        assert!(buf.len() > 0);
        // println!("Current record size is: {}", buf.len());
        let wrapper = BAMRawRecord(Cow::Borrowed(buf));
        self.push_record(&wrapper);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// TODO: Currently end user should manually call finish. Probably can be done
// with a drop. If drop and manual finish used simultaneously, crc32 and meta of
// file will be damaged.
// impl<W> Drop for Writer<W>
// where
//     W: Write + Seek,
// {
//     fn drop(&mut self) {
//         self.finish().unwrap();
//     }
// }

pub(crate) fn calc_crc_for_meta_bytes(bytes: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[ignore]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::parse_tmplt::*;
    use crate::reader::reader::*;
    use byteorder::ReadBytesExt;
    use std::io::Cursor;
    #[test]
    fn test_writer() {
        // let raw_records = vec![BAMRawRecord::default(); 2];
        // let mut buf: Vec<u8> = vec![0; SIZE_LIMIT];
        // let out = Cursor::new(&mut buf[..]);
        // let mut writer = Writer::new(out, Codecs::Gzip, 8);
        // for rec in raw_records.iter() {
        //     writer.push_record(rec);
        // }
        // let total_bytes_written = writer.finish().unwrap();
        // buf.resize(total_bytes_written as usize, 0);

        // let in_cursor = Box::new(Cursor::new(buf));
        // let mut parsing_template = ParsingTemplate::new();
        // parsing_template.set_all();
        // let mut reader = Reader::new(in_cursor, parsing_template).unwrap();
        // let mut records = reader.records();
        // let mut it = raw_records.iter();
        // while let Some(rec) = records.next_rec() {
        //     let rec_orig = it.next().unwrap();
        //     let orig_map_q = rec_orig.get_bytes(&Fields::Mapq)[0];
        //     let orig_pos = rec_orig
        //         .get_bytes(&Fields::Pos)
        //         .read_i32::<LittleEndian>()
        //         .unwrap();
        //     assert_eq!(rec.pos.unwrap(), orig_pos);
        //     assert_eq!(rec.mapq.unwrap(), orig_map_q);
        // }
    }
}
