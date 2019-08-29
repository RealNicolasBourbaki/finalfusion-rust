use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ndarray::{Array1, Array2};
use rand::{FromEntropy, Rng};
use rand_xorshift::XorShiftRng;
use reductive::pq::{QuantizeVector, ReconstructVector, TrainPQ, PQ};

use super::{CowArray, CowArray1, Storage, StorageView};
use crate::chunks::io::{ChunkIdentifier, ReadChunk, TypeId, WriteChunk};
use crate::io::{Error, ErrorKind, Result};
use crate::util::padding;

/// Quantized embedding matrix.
pub struct QuantizedArray {
    quantizer: PQ<f32>,
    quantized: Array2<u8>,
    norms: Option<Array1<f32>>,
}

impl Storage for QuantizedArray {
    fn embedding(&self, idx: usize) -> CowArray1<f32> {
        let mut reconstructed = self.quantizer.reconstruct_vector(self.quantized.row(idx));
        if let Some(ref norms) = self.norms {
            reconstructed *= norms[idx];
        }

        CowArray::Owned(reconstructed)
    }

    fn shape(&self) -> (usize, usize) {
        (self.quantized.rows(), self.quantizer.reconstructed_len())
    }
}

impl ReadChunk for QuantizedArray {
    fn read_chunk<R>(read: &mut R) -> Result<Self>
    where
        R: Read + Seek,
    {
        ChunkIdentifier::ensure_chunk_type(read, ChunkIdentifier::QuantizedArray)?;

        // Read and discard chunk length.
        read.read_u64::<LittleEndian>().map_err(|e| {
            ErrorKind::io_error("Cannot read quantized embedding matrix chunk length", e)
        })?;

        let projection = read.read_u32::<LittleEndian>().map_err(|e| {
            ErrorKind::io_error("Cannot read quantized embedding matrix projection", e)
        })? != 0;
        let read_norms = read
            .read_u32::<LittleEndian>()
            .map_err(|e| ErrorKind::io_error("Cannot read quantized embedding matrix norms", e))?
            != 0;
        let quantized_len = read
            .read_u32::<LittleEndian>()
            .map_err(|e| ErrorKind::io_error("Cannot read quantized embedding length", e))?
            as usize;
        let reconstructed_len = read
            .read_u32::<LittleEndian>()
            .map_err(|e| ErrorKind::io_error("Cannot read reconstructed embedding length", e))?
            as usize;
        let n_centroids = read
            .read_u32::<LittleEndian>()
            .map_err(|e| ErrorKind::io_error("Cannot read number of subquantizers", e))?
            as usize;
        let n_embeddings = read
            .read_u64::<LittleEndian>()
            .map_err(|e| ErrorKind::io_error("Cannot read number of quantized embeddings", e))?
            as usize;

        // Quantized storage type.
        u8::ensure_data_type(read)?;

        // Reconstructed embedding type.
        f32::ensure_data_type(read)?;

        let n_padding = padding::<f32>(read.seek(SeekFrom::Current(0)).map_err(|e| {
            ErrorKind::io_error("Cannot get file position for computing padding", e)
        })?);
        read.seek(SeekFrom::Current(n_padding as i64))
            .map_err(|e| ErrorKind::io_error("Cannot skip padding", e))?;

        let projection = if projection {
            let mut projection_vec = vec![0f32; reconstructed_len * reconstructed_len];
            read.read_f32_into::<LittleEndian>(&mut projection_vec)
                .map_err(|e| ErrorKind::io_error("Cannot read projection matrix", e))?;
            Some(
                Array2::from_shape_vec((reconstructed_len, reconstructed_len), projection_vec)
                    .map_err(Error::Shape)?,
            )
        } else {
            None
        };

        let mut quantizers = Vec::with_capacity(quantized_len);
        for _ in 0..quantized_len {
            let mut subquantizer_vec =
                vec![0f32; n_centroids * (reconstructed_len / quantized_len)];
            read.read_f32_into::<LittleEndian>(&mut subquantizer_vec)
                .map_err(|e| ErrorKind::io_error("Cannot read subquantizer", e))?;
            let subquantizer = Array2::from_shape_vec(
                (n_centroids, reconstructed_len / quantized_len),
                subquantizer_vec,
            )
            .map_err(Error::Shape)?;
            quantizers.push(subquantizer);
        }

        let norms = if read_norms {
            let mut norms_vec = vec![0f32; n_embeddings];
            read.read_f32_into::<LittleEndian>(&mut norms_vec)
                .map_err(|e| ErrorKind::io_error("Cannot read norms", e))?;
            Some(Array1::from_vec(norms_vec))
        } else {
            None
        };

        let mut quantized_embeddings_vec = vec![0u8; n_embeddings * quantized_len];
        read.read_exact(&mut quantized_embeddings_vec)
            .map_err(|e| ErrorKind::io_error("Cannot read quantized embeddings", e))?;
        let quantized =
            Array2::from_shape_vec((n_embeddings, quantized_len), quantized_embeddings_vec)
                .map_err(Error::Shape)?;

        Ok(QuantizedArray {
            quantizer: PQ::new(projection, quantizers),
            quantized,
            norms,
        })
    }
}

impl WriteChunk for QuantizedArray {
    fn chunk_identifier(&self) -> ChunkIdentifier {
        ChunkIdentifier::QuantizedArray
    }

    fn write_chunk<W>(&self, write: &mut W) -> Result<()>
    where
        W: Write + Seek,
    {
        write
            .write_u32::<LittleEndian>(ChunkIdentifier::QuantizedArray as u32)
            .map_err(|e| {
                ErrorKind::io_error(
                    "Cannot write quantized embedding matrix chunk identifier",
                    e,
                )
            })?;

        // projection (u32), use_norms (u32), quantized_len (u32),
        // reconstructed_len (u32), n_centroids (u32), rows (u64),
        // types (2 x u32 bytes), padding, projection matrix,
        // centroids, norms, quantized data.
        let n_padding = padding::<f32>(write.seek(SeekFrom::Current(0)).map_err(|e| {
            ErrorKind::io_error("Cannot get file position for computing padding", e)
        })?);
        let chunk_size = size_of::<u32>()
            + size_of::<u32>()
            + size_of::<u32>()
            + size_of::<u32>()
            + size_of::<u32>()
            + size_of::<u64>()
            + 2 * size_of::<u32>()
            + n_padding as usize
            + self.quantizer.projection().is_some() as usize
                * self.quantizer.reconstructed_len()
                * self.quantizer.reconstructed_len()
                * size_of::<f32>()
            + self.quantizer.quantized_len()
                * self.quantizer.n_quantizer_centroids()
                * (self.quantizer.reconstructed_len() / self.quantizer.quantized_len())
                * size_of::<f32>()
            + self.norms.is_some() as usize * self.quantized.rows() * size_of::<f32>()
            + self.quantized.rows() * self.quantizer.quantized_len();

        write
            .write_u64::<LittleEndian>(chunk_size as u64)
            .map_err(|e| {
                ErrorKind::io_error("Cannot write quantized embedding matrix chunk length", e)
            })?;

        write
            .write_u32::<LittleEndian>(self.quantizer.projection().is_some() as u32)
            .map_err(|e| {
                ErrorKind::io_error("Cannot write quantized embedding matrix projection", e)
            })?;
        write
            .write_u32::<LittleEndian>(self.norms.is_some() as u32)
            .map_err(|e| ErrorKind::io_error("Cannot write quantized embedding matrix norms", e))?;
        write
            .write_u32::<LittleEndian>(self.quantizer.quantized_len() as u32)
            .map_err(|e| ErrorKind::io_error("Cannot write quantized embedding length", e))?;
        write
            .write_u32::<LittleEndian>(self.quantizer.reconstructed_len() as u32)
            .map_err(|e| ErrorKind::io_error("Cannot write reconstructed embedding length", e))?;
        write
            .write_u32::<LittleEndian>(self.quantizer.n_quantizer_centroids() as u32)
            .map_err(|e| ErrorKind::io_error("Cannot write number of subquantizers", e))?;
        write
            .write_u64::<LittleEndian>(self.quantized.rows() as u64)
            .map_err(|e| ErrorKind::io_error("Cannot write number of quantized embeddings", e))?;

        // Quantized and reconstruction types.
        write
            .write_u32::<LittleEndian>(u8::type_id())
            .map_err(|e| {
                ErrorKind::io_error("Cannot write quantized embedding type identifier", e)
            })?;
        write
            .write_u32::<LittleEndian>(f32::type_id())
            .map_err(|e| {
                ErrorKind::io_error("Cannot write reconstructed embedding type identifier", e)
            })?;

        let padding = vec![0u8; n_padding as usize];
        write
            .write_all(&padding)
            .map_err(|e| ErrorKind::io_error("Cannot write padding", e))?;

        // Write projection matrix.
        if let Some(projection) = self.quantizer.projection() {
            for row in projection.outer_iter() {
                for &col in row {
                    write.write_f32::<LittleEndian>(col).map_err(|e| {
                        ErrorKind::io_error("Cannot write projection matrix component", e)
                    })?;
                }
            }
        }

        // Write subquantizers.
        for subquantizer in self.quantizer.subquantizers() {
            for row in subquantizer.outer_iter() {
                for &col in row {
                    write.write_f32::<LittleEndian>(col).map_err(|e| {
                        ErrorKind::io_error("Cannot write subquantizer component", e)
                    })?;
                }
            }
        }

        // Write norms.
        if let Some(ref norms) = self.norms {
            for row in norms.outer_iter() {
                for &col in row {
                    write.write_f32::<LittleEndian>(col).map_err(|e| {
                        ErrorKind::io_error("Cannot write norm vector component", e)
                    })?;
                }
            }
        }

        // Write quantized embedding matrix.
        for row in self.quantized.outer_iter() {
            for &col in row {
                write.write_u8(col).map_err(|e| {
                    ErrorKind::io_error("Cannot write quantized embedding matrix component", e)
                })?;
            }
        }

        Ok(())
    }
}

/// Quantizable embedding matrix.
pub trait Quantize {
    /// Quantize the embedding matrix.
    ///
    /// This method trains a quantizer for the embedding matrix and
    /// then quantizes the matrix using this quantizer.
    ///
    /// The xorshift PRNG is used for picking the initial quantizer
    /// centroids.
    fn quantize<T>(
        &self,
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        normalize: bool,
    ) -> QuantizedArray
    where
        T: TrainPQ<f32>,
    {
        self.quantize_using::<T, _>(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            normalize,
            &mut XorShiftRng::from_entropy(),
        )
    }

    /// Quantize the embedding matrix using the provided RNG.
    ///
    /// This method trains a quantizer for the embedding matrix and
    /// then quantizes the matrix using this quantizer.
    fn quantize_using<T, R>(
        &self,
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        normalize: bool,
        rng: &mut R,
    ) -> QuantizedArray
    where
        T: TrainPQ<f32>,
        R: Rng;
}

impl<S> Quantize for S
where
    S: StorageView,
{
    /// Quantize the embedding matrix.
    ///
    /// This method trains a quantizer for the embedding matrix and
    /// then quantizes the matrix using this quantizer.
    fn quantize_using<T, R>(
        &self,
        n_subquantizers: usize,
        n_subquantizer_bits: u32,
        n_iterations: usize,
        n_attempts: usize,
        normalize: bool,
        rng: &mut R,
    ) -> QuantizedArray
    where
        T: TrainPQ<f32>,
        R: Rng,
    {
        let (embeds, norms) = if normalize {
            let norms = self.view().outer_iter().map(|e| e.dot(&e).sqrt()).collect();
            let mut normalized = self.view().to_owned();
            for (mut embedding, &norm) in normalized.outer_iter_mut().zip(&norms) {
                embedding /= norm;
            }
            (CowArray::Owned(normalized), Some(norms))
        } else {
            (CowArray::Borrowed(self.view()), None)
        };

        let quantizer = T::train_pq_using(
            n_subquantizers,
            n_subquantizer_bits,
            n_iterations,
            n_attempts,
            embeds.as_view(),
            rng,
        );

        let quantized = quantizer.quantize_batch(embeds.as_view());

        QuantizedArray {
            quantizer,
            quantized,
            norms,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, SeekFrom};

    use byteorder::{LittleEndian, ReadBytesExt};
    use ndarray::Array2;
    use reductive::pq::PQ;

    use crate::chunks::io::{ReadChunk, WriteChunk};
    use crate::chunks::storage::{NdArray, Quantize, QuantizedArray};

    const N_ROWS: usize = 100;
    const N_COLS: usize = 100;

    fn test_ndarray() -> NdArray {
        let test_data = Array2::from_shape_fn((N_ROWS, N_COLS), |(r, c)| {
            r as f32 * N_COLS as f32 + c as f32
        });

        NdArray(test_data)
    }

    fn test_quantized_array(norms: bool) -> QuantizedArray {
        let ndarray = test_ndarray();
        ndarray.quantize::<PQ<f32>>(10, 4, 5, 1, norms)
    }

    fn read_chunk_size(read: &mut impl Read) -> u64 {
        // Skip identifier.
        read.read_u32::<LittleEndian>().unwrap();

        // Return chunk length.
        read.read_u64::<LittleEndian>().unwrap()
    }

    #[test]
    fn quantized_array_correct_chunk_size() {
        let check_arr = test_quantized_array(false);
        let mut cursor = Cursor::new(Vec::new());
        check_arr.write_chunk(&mut cursor).unwrap();
        cursor.seek(SeekFrom::Start(0)).unwrap();

        let chunk_size = read_chunk_size(&mut cursor);
        assert_eq!(
            cursor.read_to_end(&mut Vec::new()).unwrap(),
            chunk_size as usize
        );
    }

    #[test]
    fn quantized_array_norms_correct_chunk_size() {
        let check_arr = test_quantized_array(true);
        let mut cursor = Cursor::new(Vec::new());
        check_arr.write_chunk(&mut cursor).unwrap();
        cursor.seek(SeekFrom::Start(0)).unwrap();

        let chunk_size = read_chunk_size(&mut cursor);
        assert_eq!(
            cursor.read_to_end(&mut Vec::new()).unwrap(),
            chunk_size as usize
        );
    }

    #[test]
    fn quantized_array_read_write_roundtrip() {
        let check_arr = test_quantized_array(true);
        let mut cursor = Cursor::new(Vec::new());
        check_arr.write_chunk(&mut cursor).unwrap();
        cursor.seek(SeekFrom::Start(0)).unwrap();
        let arr = QuantizedArray::read_chunk(&mut cursor).unwrap();
        assert_eq!(arr.quantizer, check_arr.quantizer);
        assert_eq!(arr.quantized, check_arr.quantized);
    }
}