//! 本机 steamclient64 (sha 61dd80e8…) 已钉偏移; 仅作常量, 本 crate 不写进程内存.
//!
//! 证据: docs/plan/scratch/m5-steamclient-recon.md §3.

/// 注入用的假 package id (上游 kInjectedPackageId).
pub const INJECTED_PACKAGE_ID: u32 = 0;

/// package0 查询用的 access token (上游硬编码, 非用户密钥).
pub const INJECTED_PACKAGE_ACCESS_TOKEN: u64 = 10_660_652_434_190_618_804;

/// `EPackageStatus::Available`.
pub const PACKAGE_STATUS_AVAILABLE: u32 = 0;
/// `EPackageStatus::Invalid` (空构造初值).
pub const PACKAGE_STATUS_INVALID: u32 = 3;

/// `EAppReleaseState::Released`.
pub const APP_RELEASE_STATE_RELEASED: u32 = 4;

/// `AppOwnership` 出参大致长度 (字节).
pub const APP_OWNERSHIP_SIZE: usize = 0x38;

pub mod app_ownership {
    pub const PACKAGE_ID: usize = 0x00;
    pub const RELEASE_STATE: usize = 0x04;
    pub const STEAM_ID32: usize = 0x08;
    pub const MASTER_SUBSCRIPTION_APP_ID: usize = 0x0C;
    pub const TRIAL_SECONDS: usize = 0x10;
    pub const EXIST_IN_PACKAGE_NUMS: usize = 0x14;
    pub const PURCHASE_COUNTRY_CODE: usize = 0x18;
    pub const TIME_STAMP: usize = 0x1C;
    pub const TIME_EXPIRE: usize = 0x20;
    pub const B_OWNS_LICENSE: usize = 0x24;
    pub const B_LICENSE_EXPIRED: usize = 0x25;
    pub const B_IS_PERMANENT: usize = 0x26;
    pub const B_LOW_VIOLENCE: usize = 0x27;
    pub const B_FREE_LICENSE: usize = 0x28;
}

pub mod package_info {
    pub const PACKAGE_ID: usize = 0x00;
    pub const CHANGE_NUMBER: usize = 0x04;
    pub const PICS_TOKEN: usize = 0x08;
    pub const STATUS: usize = 0x18;
    /// `AppIdVec.m_Memory.m_pMemory`
    pub const APP_ID_VEC_MEMORY: usize = 0x40;
    /// `AppIdVec.m_Size`
    pub const APP_ID_VEC_SIZE: usize = 0x50;
}

/// `CUtlMemory` / 整段 `CUtlVector` 上传给 `CUtlMemoryGrow` 时的字段.
pub mod utl_vector {
    pub const MEMORY_PTR: usize = 0x00;
    pub const ALLOCATION_COUNT: usize = 0x08;
    pub const GROW_SIZE: usize = 0x0C;
    pub const SIZE: usize = 0x10;
    pub const ELEMENT_STRIDE_APP_ID: usize = 4;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_min_write_offsets_are_distinct() {
        assert_eq!(app_ownership::PACKAGE_ID, 0);
        assert_eq!(app_ownership::RELEASE_STATE, 4);
        assert_eq!(app_ownership::EXIST_IN_PACKAGE_NUMS, 0x14);
        assert_eq!(app_ownership::B_OWNS_LICENSE, 0x24);
        assert_eq!(app_ownership::B_FREE_LICENSE, 0x28);
        const {
            assert!(app_ownership::B_FREE_LICENSE < APP_OWNERSHIP_SIZE);
        }
    }

    #[test]
    fn package_app_id_vec_matches_recon() {
        assert_eq!(package_info::STATUS, 0x18);
        assert_eq!(package_info::APP_ID_VEC_MEMORY, 0x40);
        assert_eq!(package_info::APP_ID_VEC_SIZE, 0x50);
        assert_eq!(
            package_info::APP_ID_VEC_SIZE - package_info::APP_ID_VEC_MEMORY,
            utl_vector::SIZE
        );
    }
}
