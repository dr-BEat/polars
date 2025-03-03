mod args;
pub(crate) mod multiple_keys;
pub(super) mod single_keys;
mod single_keys_dispatch;
mod single_keys_inner;
mod single_keys_left;
mod single_keys_outer;
#[cfg(feature = "semi_anti_join")]
mod single_keys_semi_anti;
pub(super) mod sort_merge;
mod zip_outer;

use std::fmt::{Debug, Display, Formatter};
use std::hash::{BuildHasher, Hash, Hasher};

use ahash::RandomState;
pub use args::*;
#[cfg(feature = "chunked_ids")]
use arrow::Either;
use hashbrown::hash_map::{Entry, RawEntryMut};
use hashbrown::HashMap;
use polars_arrow::utils::CustomIterTools;
use rayon::prelude::*;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "asof_join")]
pub(crate) use single_keys::build_tables;
#[cfg(feature = "asof_join")]
pub(crate) use single_keys_dispatch::prepare_bytes;
use single_keys_left::*;
use single_keys_outer::*;
#[cfg(feature = "semi_anti_join")]
use single_keys_semi_anti::*;
pub use sort_merge::*;
pub(crate) use zip_outer::*;

pub use self::multiple_keys::private_left_join_multiple_keys;
use crate::datatypes::PlHashMap;
use crate::frame::group_by::hashing::HASHMAP_INIT_SIZE;
pub use crate::frame::hash_join::multiple_keys::{
    _inner_join_multiple_keys, _left_join_multiple_keys, _outer_join_multiple_keys,
};
#[cfg(feature = "semi_anti_join")]
pub use crate::frame::hash_join::multiple_keys::{
    _left_anti_multiple_keys, _left_semi_multiple_keys,
};
use crate::hashing::{
    create_hash_and_keys_threaded_vectorized, prepare_hashed_relation_threaded, this_partition,
    AsU64, BytesHash,
};
use crate::prelude::*;
use crate::utils::{_set_partition_size, slice_slice, split_ca};
use crate::POOL;

pub fn default_join_ids() -> ChunkJoinOptIds {
    #[cfg(feature = "chunked_ids")]
    {
        Either::Left(vec![])
    }
    #[cfg(not(feature = "chunked_ids"))]
    {
        vec![]
    }
}

macro_rules! det_hash_prone_order {
    ($self:expr, $other:expr) => {{
        // The shortest relation will be used to create a hash table.
        let left_first = $self.len() > $other.len();
        let a;
        let b;
        if left_first {
            a = $self;
            b = $other;
        } else {
            b = $self;
            a = $other;
        }

        (a, b, !left_first)
    }};
}

pub(super) use det_hash_prone_order;
#[cfg(feature = "performant")]
use polars_arrow::conversion::primitive_to_vec;
use polars_utils::hash_to_partition;

use crate::series::IsSorted;

/// If Categorical types are created without a global string cache or under
/// a different global string cache the mapping will be incorrect.
#[cfg(feature = "dtype-categorical")]
pub fn _check_categorical_src(l: &DataType, r: &DataType) -> PolarsResult<()> {
    if let (DataType::Categorical(Some(l)), DataType::Categorical(Some(r))) = (l, r) {
        polars_ensure!(l.same_src(r), string_cache_mismatch);
    }
    Ok(())
}

pub(crate) unsafe fn get_hash_tbl_threaded_join_partitioned<Item>(
    h: u64,
    hash_tables: &[Item],
    len: u64,
) -> &Item {
    let i = hash_to_partition(h, len as usize);
    hash_tables.get_unchecked(i)
}

#[allow(clippy::type_complexity)]
unsafe fn get_hash_tbl_threaded_join_mut_partitioned<T, H>(
    h: u64,
    hash_tables: &mut [HashMap<T, (bool, Vec<IdxSize>), H>],
    len: u64,
) -> &mut HashMap<T, (bool, Vec<IdxSize>), H> {
    let i = hash_to_partition(h, len as usize);
    hash_tables.get_unchecked_mut(i)
}

pub fn _join_suffix_name(name: &str, suffix: &str) -> String {
    format!("{name}{suffix}")
}

/// Utility method to finish a join.
#[doc(hidden)]
pub fn _finish_join(
    mut df_left: DataFrame,
    mut df_right: DataFrame,
    suffix: Option<&str>,
) -> PolarsResult<DataFrame> {
    let mut left_names = PlHashSet::with_capacity(df_left.width());

    df_left.columns.iter().for_each(|series| {
        left_names.insert(series.name());
    });

    let mut rename_strs = Vec::with_capacity(df_right.width());

    df_right.columns.iter().for_each(|series| {
        if left_names.contains(series.name()) {
            rename_strs.push(series.name().to_owned())
        }
    });
    let suffix = suffix.unwrap_or("_right");

    for name in rename_strs {
        df_right.rename(&name, &_join_suffix_name(&name, suffix))?;
    }

    drop(left_names);
    df_left.hstack_mut(&df_right.columns)?;
    Ok(df_left)
}

impl DataFrame {
    /// # Safety
    /// Join tuples must be in bounds
    #[cfg(feature = "chunked_ids")]
    unsafe fn create_left_df_chunked(&self, chunk_ids: &[ChunkId], left_join: bool) -> DataFrame {
        if left_join && chunk_ids.len() == self.height() {
            self.clone()
        } else {
            // left join keys are in ascending order
            let sorted = if left_join {
                IsSorted::Ascending
            } else {
                IsSorted::Not
            };
            self.take_chunked_unchecked(chunk_ids, sorted)
        }
    }

    /// # Safety
    /// Join tuples must be in bounds
    pub unsafe fn _create_left_df_from_slice(
        &self,
        join_tuples: &[IdxSize],
        left_join: bool,
        sorted_tuple_idx: bool,
    ) -> DataFrame {
        if left_join && join_tuples.len() == self.height() {
            self.clone()
        } else {
            // left join tuples are always in ascending order
            let sorted = if left_join || sorted_tuple_idx {
                IsSorted::Ascending
            } else {
                IsSorted::Not
            };

            self._take_unchecked_slice_sorted(join_tuples, true, sorted)
        }
    }

    #[cfg(not(feature = "chunked_ids"))]
    pub fn _finish_left_join(
        &self,
        ids: LeftJoinIds,
        other: &DataFrame,
        args: JoinArgs,
    ) -> PolarsResult<DataFrame> {
        let (left_idx, right_idx) = ids;
        let materialize_left = || {
            let mut left_idx = &*left_idx;
            if let Some((offset, len)) = args.slice {
                left_idx = slice_slice(left_idx, offset, len);
            }
            unsafe { self._create_left_df_from_slice(left_idx, true, true) }
        };

        let materialize_right = || {
            let mut right_idx = &*right_idx;
            if let Some((offset, len)) = args.slice {
                right_idx = slice_slice(right_idx, offset, len);
            }
            unsafe {
                other.take_opt_iter_unchecked(
                    right_idx.iter().map(|opt_i| opt_i.map(|i| i as usize)),
                )
            }
        };
        let (df_left, df_right) = POOL.join(materialize_left, materialize_right);

        _finish_join(df_left, df_right, args.suffix.as_deref())
    }

    #[cfg(feature = "chunked_ids")]
    pub fn _finish_left_join(
        &self,
        ids: LeftJoinIds,
        other: &DataFrame,
        args: JoinArgs,
    ) -> PolarsResult<DataFrame> {
        let suffix = &args.suffix;
        let slice = args.slice;
        let (left_idx, right_idx) = ids;
        let materialize_left = || match left_idx {
            ChunkJoinIds::Left(left_idx) => {
                let mut left_idx = &*left_idx;
                if let Some((offset, len)) = slice {
                    left_idx = slice_slice(left_idx, offset, len);
                }
                unsafe { self._create_left_df_from_slice(left_idx, true, true) }
            },
            ChunkJoinIds::Right(left_idx) => {
                let mut left_idx = &*left_idx;
                if let Some((offset, len)) = slice {
                    left_idx = slice_slice(left_idx, offset, len);
                }
                unsafe { self.create_left_df_chunked(left_idx, true) }
            },
        };

        let materialize_right = || match right_idx {
            ChunkJoinOptIds::Left(right_idx) => {
                let mut right_idx = &*right_idx;
                if let Some((offset, len)) = slice {
                    right_idx = slice_slice(right_idx, offset, len);
                }
                unsafe {
                    other.take_opt_iter_unchecked(
                        right_idx.iter().map(|opt_i| opt_i.map(|i| i as usize)),
                    )
                }
            },
            ChunkJoinOptIds::Right(right_idx) => {
                let mut right_idx = &*right_idx;
                if let Some((offset, len)) = slice {
                    right_idx = slice_slice(right_idx, offset, len);
                }
                unsafe { other.take_opt_chunked_unchecked(right_idx) }
            },
        };
        let (df_left, df_right) = POOL.join(materialize_left, materialize_right);

        _finish_join(df_left, df_right, suffix.as_deref())
    }

    pub fn _left_join_from_series(
        &self,
        other: &DataFrame,
        s_left: &Series,
        s_right: &Series,
        args: JoinArgs,
        verbose: bool,
    ) -> PolarsResult<DataFrame> {
        #[cfg(feature = "dtype-categorical")]
        _check_categorical_src(s_left.dtype(), s_right.dtype())?;

        // ensure that the chunks are aligned otherwise we go OOB
        let mut left = self.clone();
        let mut s_left = s_left.clone();
        let mut right = other.clone();
        let mut s_right = s_right.clone();
        if left.should_rechunk() {
            left.as_single_chunk_par();
            s_left = s_left.rechunk();
        }
        if right.should_rechunk() {
            right.as_single_chunk_par();
            s_right = s_right.rechunk();
        }
        let ids = sort_or_hash_left(&s_left, &s_right, verbose, args.validation)?;
        left._finish_left_join(ids, &right.drop(s_right.name()).unwrap(), args)
    }

    #[cfg(feature = "semi_anti_join")]
    /// # Safety
    /// `idx` must be in bounds
    pub unsafe fn _finish_anti_semi_join(
        &self,
        mut idx: &[IdxSize],
        slice: Option<(i64, usize)>,
    ) -> DataFrame {
        if let Some((offset, len)) = slice {
            idx = slice_slice(idx, offset, len);
        }
        // idx from anti-semi join should always be sorted
        self._take_unchecked_slice_sorted(idx, true, IsSorted::Ascending)
    }

    #[cfg(feature = "semi_anti_join")]
    pub fn _semi_anti_join_from_series(
        &self,
        s_left: &Series,
        s_right: &Series,
        slice: Option<(i64, usize)>,
        anti: bool,
    ) -> PolarsResult<DataFrame> {
        #[cfg(feature = "dtype-categorical")]
        _check_categorical_src(s_left.dtype(), s_right.dtype())?;

        let idx = s_left.hash_join_semi_anti(s_right, anti);
        // Safety:
        // indices are in bounds
        Ok(unsafe { self._finish_anti_semi_join(&idx, slice) })
    }
    pub fn _outer_join_from_series(
        &self,
        other: &DataFrame,
        s_left: &Series,
        s_right: &Series,
        args: JoinArgs,
    ) -> PolarsResult<DataFrame> {
        #[cfg(feature = "dtype-categorical")]
        _check_categorical_src(s_left.dtype(), s_right.dtype())?;

        // store this so that we can keep original column order.
        let join_column_index = self.iter().position(|s| s.name() == s_left.name()).unwrap();

        // Get the indexes of the joined relations
        let opt_join_tuples = s_left.hash_join_outer(s_right, args.validation)?;
        let mut opt_join_tuples = &*opt_join_tuples;

        if let Some((offset, len)) = args.slice {
            opt_join_tuples = slice_slice(opt_join_tuples, offset, len);
        }

        // Take the left and right dataframes by join tuples
        let (mut df_left, df_right) = POOL.join(
            || unsafe {
                self.drop(s_left.name()).unwrap().take_opt_iter_unchecked(
                    opt_join_tuples
                        .iter()
                        .map(|(left, _right)| left.map(|i| i as usize)),
                )
            },
            || unsafe {
                other.drop(s_right.name()).unwrap().take_opt_iter_unchecked(
                    opt_join_tuples
                        .iter()
                        .map(|(_left, right)| right.map(|i| i as usize)),
                )
            },
        );

        let mut s = s_left
            .to_physical_repr()
            .zip_outer_join_column(&s_right.to_physical_repr(), opt_join_tuples);
        s.rename(s_left.name());
        let s = match s_left.dtype() {
            #[cfg(feature = "dtype-categorical")]
            DataType::Categorical(_) => {
                let ca_left = s_left.categorical().unwrap();
                let new_rev_map = ca_left.merge_categorical_map(s_right.categorical().unwrap())?;
                let logical = s.u32().unwrap().clone();
                // safety:
                // categorical maps are merged
                unsafe {
                    CategoricalChunked::from_cats_and_rev_map_unchecked(logical, new_rev_map)
                        .into_series()
                }
            },
            dt @ DataType::Datetime(_, _)
            | dt @ DataType::Time
            | dt @ DataType::Date
            | dt @ DataType::Duration(_) => s.cast(dt).unwrap(),
            _ => s,
        };

        unsafe { df_left.get_columns_mut().insert(join_column_index, s) };
        _finish_join(df_left, df_right, args.suffix.as_deref())
    }
}
