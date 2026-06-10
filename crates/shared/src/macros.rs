//! Internal helper macros for defining id enums with parallel data tables.

/// Defines a `#[repr(uN)]` id enum together with its parallel static data
/// table.
///
/// Discriminants are assigned sequentially from 0 in declaration order, so
/// `TABLE[id as usize]` is always the row for `id` — the macro keeps enum and
/// table in sync by construction (one entry produces both).
macro_rules! id_table {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $Enum:ident($repr:ty), $tvis:vis table $TABLE:ident: $Data:ty {
            $($(#[$vmeta:meta])* $Variant:ident => $row:expr,)+
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
            serde::Serialize, serde::Deserialize,
        )]
        #[repr($repr)]
        $vis enum $Enum {
            $($(#[$vmeta])* $Variant,)+
        }

        $tvis static $TABLE: &[$Data] = &[$($row,)+];

        impl $Enum {
            /// Every variant, in discriminant order.
            pub const ALL: &'static [$Enum] = &[$($Enum::$Variant,)+];
            /// Number of variants.
            pub const COUNT: usize = $Enum::ALL.len();

            /// The static data row for this id.
            #[inline]
            pub fn data(self) -> &'static $Data {
                &$TABLE[self as usize]
            }

            /// Inverse of `id as $repr`; `None` for out-of-range values.
            #[inline]
            pub fn from_repr(v: $repr) -> Option<Self> {
                Self::ALL.get(v as usize).copied()
            }
        }
    };
}
pub(crate) use id_table;
