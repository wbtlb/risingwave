// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use risingwave_common::array::*;
use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::types::*;

use crate::expr::ExpressionRef;
use crate::vector_op::agg::aggregator::Aggregator;

const INDEX_BITS: u8 = 14; // number of bits used for finding the index of each 64-bit hash
const NUM_OF_REGISTERS: usize = 1 << INDEX_BITS; // number of indices available
const COUNT_BITS: u8 = 64 - INDEX_BITS; // number of non-index bits in each 64-bit hash

// Approximation for bias correction for 16384 registers. See "HyperLogLog: the analysis of a
// near-optimal cardinality estimation algorithm" by Philippe Flajolet et al.
const BIAS_CORRECTION: f64 = 0.72125;

/// `ApproxCountDistinct` approximates the count of non-null rows using `HyperLogLog`. The
/// estimation error for `HyperLogLog` is 1.04/sqrt(num of registers). With 2^14 registers this
/// is ~1/128.
#[derive(Clone)]
pub struct ApproxCountDistinct {
    return_type: DataType,
    input_col_idx: usize,
    registers: [u8; NUM_OF_REGISTERS],
    filter: ExpressionRef,
}

impl ApproxCountDistinct {
    pub fn new(return_type: DataType, input_col_idx: usize, filter: ExpressionRef) -> Self {
        Self {
            return_type,
            input_col_idx,
            registers: [0; NUM_OF_REGISTERS],
            filter,
        }
    }

    /// Adds the count of the datum's hash into the register, if it is greater than the existing
    /// count at the register
    fn add_datum(&mut self, datum_ref: DatumRef) {
        if datum_ref.is_none() {
            return;
        }

        let scalar_impl = datum_ref.unwrap().into_scalar_impl();
        let hash = self.get_hash(scalar_impl);

        let index = (hash as usize) & (NUM_OF_REGISTERS - 1); // Index is based on last few bits
        let count = self.count_hash(hash);

        if count > self.registers[index] {
            self.registers[index] = count;
        }
    }

    /// Calculate the hash of the `scalar_impl` using Rust's default hasher
    /// Perhaps a different hash like Murmur2 could be used instead for optimization?
    fn get_hash(&self, scalar_impl: ScalarImpl) -> u64 {
        let mut hasher = DefaultHasher::new();
        scalar_impl.hash(&mut hasher);
        hasher.finish()
    }

    /// Counts the number of trailing zeroes plus 1 in the non-index bits of the hash
    fn count_hash(&self, mut hash: u64) -> u8 {
        hash >>= INDEX_BITS; // Ignore bits used as index for the hash
        hash |= 1 << COUNT_BITS; // To allow hash to terminate if it is all 0s

        (hash.trailing_zeros() + 1) as u8
    }

    /// Calculates the bias-corrected harmonic mean of the registers to get the approximate count
    fn calculate_result(&self) -> i64 {
        let m = NUM_OF_REGISTERS as f64;
        let mut mean = 0.0;

        // Get harmonic mean of all the counts in results
        for count in self.registers.iter() {
            mean += 1.0 / ((1 << *count) as f64);
        }

        let raw_estimate = BIAS_CORRECTION * m * m / mean;

        // If raw_estimate is not much bigger than m and some registers have value 0, set answer to
        // m * log(m/V) where V is the number of registers with value 0
        let answer = if raw_estimate <= 2.5 * m {
            let mut zero_registers: f64 = 0.0;
            for i in self.registers.iter() {
                if *i == 0 {
                    zero_registers += 1.0;
                }
            }

            if zero_registers == 0.0 {
                raw_estimate
            } else {
                m * (m.log2() - (zero_registers.log2()))
            }
        } else {
            raw_estimate
        };

        answer as i64
    }

    /// `apply_filter_on_row` apply a filter on the given row, and return if the row satisfies the
    /// filter or not # SAFETY
    /// the given row must be visible
    fn apply_filter_on_row(&self, input: &DataChunk, row_id: usize) -> Result<bool> {
        let (row, visible) = input.row_at(row_id)?;
        assert!(visible);
        let filter_res = if let Some(ScalarImpl::Bool(v)) = self.filter.eval_row(&Row::from(row))? {
            v
        } else {
            false
        };
        Ok(filter_res)
    }
}

impl Aggregator for ApproxCountDistinct {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    fn update_single(&mut self, input: &DataChunk, row_id: usize) -> Result<()> {
        let filter_res = self.apply_filter_on_row(input, row_id)?;
        if filter_res {
            let array = input.column_at(self.input_col_idx).array_ref();
            let datum_ref = array.value_at(row_id);
            self.add_datum(datum_ref);
        }
        Ok(())
    }

    fn update_multi(
        &mut self,
        input: &DataChunk,
        start_row_id: usize,
        end_row_id: usize,
    ) -> Result<()> {
        let array = input.column_at(self.input_col_idx).array_ref();
        for row_id in start_row_id..end_row_id {
            let filter_res = self.apply_filter_on_row(input, row_id)?;
            if filter_res {
                let datum_ref = array.value_at(row_id);
                self.add_datum(datum_ref);
            }
        }
        Ok(())
    }

    fn output(&mut self, builder: &mut ArrayBuilderImpl) -> Result<()> {
        let result = self.calculate_result();
        self.registers = [0; NUM_OF_REGISTERS];
        match builder {
            ArrayBuilderImpl::Int64(b) => b.append(Some(result)).map_err(Into::into),
            _ => Err(ErrorCode::InternalError("Unexpected builder for count(*).".into()).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use risingwave_common::array::column::Column;
    use risingwave_common::array::{
        ArrayBuilder, ArrayBuilderImpl, DataChunk, I32Array, I64ArrayBuilder,
    };
    use risingwave_common::types::{DataType, ScalarImpl};

    use crate::expr::{Expression, LiteralExpression};
    use crate::vector_op::agg::aggregator::Aggregator;
    use crate::vector_op::agg::approx_count_distinct::ApproxCountDistinct;

    fn generate_data_chunk(size: usize, start: i32) -> DataChunk {
        let mut lhs = vec![];
        for i in start..((size as i32) + start) {
            lhs.push(Some(i));
        }

        let col1 = Column::new(
            I32Array::from_slice(&lhs)
                .map(|x| Arc::new(x.into()))
                .unwrap(),
        );

        DataChunk::new(vec![col1], size)
    }

    #[test]
    fn test_update_single() {
        let inputs_size: [usize; 3] = [20000, 10000, 5000];
        let inputs_start: [i32; 3] = [0, 20000, 30000];

        let mut agg = ApproxCountDistinct::new(
            DataType::Int64,
            0,
            Arc::from(
                LiteralExpression::new(DataType::Boolean, Some(ScalarImpl::Bool(true))).boxed(),
            ),
        );
        let mut builder = ArrayBuilderImpl::Int64(I64ArrayBuilder::new(3));

        for i in 0..3 {
            let data_chunk = generate_data_chunk(inputs_size[i], inputs_start[i]);
            for row_id in 0..data_chunk.cardinality() {
                agg.update_single(&data_chunk, row_id).unwrap();
            }
            agg.output(&mut builder).unwrap();
        }

        let array = builder.finish().unwrap();
        assert_eq!(array.len(), 3);
    }

    #[test]
    fn test_update_multi() {
        let inputs_size: [usize; 3] = [20000, 10000, 5000];
        let inputs_start: [i32; 3] = [0, 20000, 30000];

        let mut agg = ApproxCountDistinct::new(
            DataType::Int64,
            0,
            Arc::from(
                LiteralExpression::new(DataType::Boolean, Some(ScalarImpl::Bool(true))).boxed(),
            ),
        );
        let mut builder = ArrayBuilderImpl::Int64(I64ArrayBuilder::new(3));

        for i in 0..3 {
            let data_chunk = generate_data_chunk(inputs_size[i], inputs_start[i]);
            agg.update_multi(&data_chunk, 0, data_chunk.cardinality())
                .unwrap();
            agg.output(&mut builder).unwrap();
        }

        let array = builder.finish().unwrap();
        assert_eq!(array.len(), 3);
    }
}
