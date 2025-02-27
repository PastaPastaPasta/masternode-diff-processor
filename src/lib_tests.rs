#[cfg(test)]
pub mod tests {
    extern crate libc;
    extern crate reqwest;
    use crate::processing::processor_cache::MasternodeProcessorCache;
    use crate::processing::{MNListDiffResult, QRInfoResult};
    use crate::{
        process_mnlistdiff_from_message, processor_create_cache, register_processor,
        unwrap_or_diff_processing_failure, unwrap_or_qr_processing_failure, unwrap_or_return,
        MasternodeProcessor, ProcessingError,
    };
    use byte::BytesExt;
    use dash_spv_ffi::ffi::boxer::boxed;
    use dash_spv_ffi::ffi::from::FromFFI;
    use dash_spv_ffi::ffi::to::ToFFI;
    use dash_spv_ffi::ffi::unboxer::unbox_any;
    use dash_spv_ffi::types;
    use dash_spv_models::common::chain_type::{ChainType, IHaveChainSettings};
    use dash_spv_models::common::LLMQType;
    use dash_spv_models::{llmq, masternode};
    use dash_spv_primitives::consensus::encode;
    use dash_spv_primitives::crypto::byte_util::{
        BytesDecodable, Reversable, UInt256, UInt384, UInt768,
    };
    use dash_spv_primitives::hashes::hex::{FromHex, ToHex};
    use serde::{Deserialize, Serialize};
    use std::io::Read;
    use std::ptr::null_mut;
    use std::{env, fs, slice};
    use crate::tests::block_store::init_testnet_store;

    // This regex can be used to omit timestamp etc. while replacing after paste from xcode console log
    // So it's bascically cut off such an expression "2022-09-11 15:31:59.445343+0300 DashSync_Example[41749:2762015]"
    // (\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2}).(\d{6})\+(\d{4}) DashSync_Example\[(\d{5}):(\d{7})\]

    // This regex + replace can be used to transform string like
    // "000000000000001b33b86b6a167d37e3fcc6ba53e02df3cb06e3f272bb89dd7d" => 1092744,
    // into string like
    // ("0000013c21c2dc49704656ffc5adfd9c58506ac4c9556391d6f2d3d8db579233", 796617,),
    // which is very handy
    // ("[0-9A-Fa-f]{64}") => (\d+,)
    // ($1, $2),

    #[derive(Debug)]
    pub struct FFIContext<'a> {
        pub chain: ChainType,
        pub cache: &'a mut MasternodeProcessorCache,
        // TODO:: make it initialized from json file with blocks
        pub blocks: Vec<MerkleBlock>,
    }

    impl<'a> FFIContext<'a> {
        pub fn block_for_hash(&self, hash: UInt256) -> Option<&MerkleBlock> {
            self.blocks.iter().find(|block| block.hash == hash)
        }
        pub fn block_for_height(&self, height: u32) -> Option<&MerkleBlock> {
            self.blocks.iter().find(|block| block.height == height)
        }

        pub fn genesis_as_ptr(&self) -> *const u8 {
            self.chain.genesis_hash().0.as_ptr()
        }
    }

    #[derive(Debug, Copy, Clone)]
    pub struct MerkleBlock {
        pub hash: UInt256,
        pub height: u32,
        pub merkleroot: UInt256,
    }

    #[derive(Serialize, Deserialize)]
    struct Block {
        pub hash: String,
        pub size: i64,
        pub height: i64,
        pub version: i64,
        pub merkleroot: String,
        pub tx: Vec<String>,
        pub time: i64,
        pub nonce: i64,
        pub bits: String,
        pub difficulty: f64,
        pub chainwork: String,
        pub confirmations: i64,
        pub previousblockhash: String,
        pub nextblockhash: String,
        pub reward: String,
        #[serde(rename = "isMainChain")]
        pub is_main_chain: bool,
        #[serde(rename = "poolInfo")]
        pub pool_info: PoolInfo,
    }
    #[derive(Serialize, Deserialize)]
    struct PoolInfo {}

    pub struct AggregationInfo {
        pub public_key: UInt384,
        pub digest: UInt256,
    }
    pub fn get_block_from_insight_by_hash(hash: UInt256) -> Option<MerkleBlock> {
        let path = format!("https://testnet-insight.dashevo.org/insight-api-dash/block/{}", hash.clone().reversed().0.to_hex().as_str());
        request_block(path)
    }
    pub fn get_block_from_insight_by_height(height: u32) -> Option<MerkleBlock> {
        let path = format!("https://testnet-insight.dashevo.org/insight-api-dash/block/{}", height);
        request_block(path)
    }

    pub fn request_block(path: String) -> Option<MerkleBlock> {
        println!("request_block: {}", path.as_str());
        match reqwest::blocking::get(path.as_str()) {
            Ok(response) => match response.json::<serde_json::Value>() {
                Ok(json) => {
                    let block: Block = serde_json::from_value(json).unwrap();
                    let merkle_block = MerkleBlock {
                        hash: UInt256::from_hex(block.hash.as_str()).unwrap().reversed(),
                        height: block.height as u32,
                        merkleroot: UInt256::from_hex(block.merkleroot.as_str()).unwrap()
                    };
                    println!("request_block: {}", path.as_str());
                    Some(merkle_block)
                },
                Err(err) => {
                    println!("{}", err);
                    None
                },
            },
            Err(err) => {
                println!("{}", err);
                None
            },
        }
    }

    /// This is convenience Core v0.17 method for use in tests which doesn't involve cross-FFI calls
    pub fn process_mnlistdiff_from_message_internal(
        message_arr: *const u8,
        message_length: usize,
        use_insight_as_backup: bool,
        genesis_hash: *const u8,
        processor: *mut MasternodeProcessor,
        cache: *mut MasternodeProcessorCache,
        context: *const std::ffi::c_void,
    ) -> MNListDiffResult {
        let processor = unsafe { &mut *processor };
        let cache = unsafe { &mut *cache };
        println!(
            "process_mnlistdiff_from_message_internal.start: {:?}",
            std::time::Instant::now()
        );
        processor.opaque_context = context;
        processor.use_insight_as_backup = use_insight_as_backup;
        processor.genesis_hash = genesis_hash;
        let message: &[u8] = unsafe { slice::from_raw_parts(message_arr, message_length as usize) };
        let list_diff =
            unwrap_or_diff_processing_failure!(llmq::MNListDiff::new(message, &mut 0, |hash| {
                processor.lookup_block_height_by_hash(hash)
            }));
        let result = processor.get_list_diff_result_internal_with_base_lookup(list_diff, cache);
        println!(
            "process_mnlistdiff_from_message_internal.finish: {:?} {:#?}",
            std::time::Instant::now(),
            result
        );
        result
    }

    /// This is convenience Core v0.18 method for use in tests which doesn't involve cross-FFI calls
    pub fn process_qrinfo_from_message_internal(
        message: *const u8,
        message_length: usize,
        use_insight_as_backup: bool,
        genesis_hash: *const u8,
        processor: *mut MasternodeProcessor,
        cache: *mut MasternodeProcessorCache,
        context: *const std::ffi::c_void,
    ) -> QRInfoResult {
        println!("process_qrinfo_from_message: {:?} {:?}", processor, cache);
        let message: &[u8] = unsafe { slice::from_raw_parts(message, message_length as usize) };
        let processor = unsafe { &mut *processor };
        processor.opaque_context = context;
        processor.use_insight_as_backup = use_insight_as_backup;
        processor.genesis_hash = genesis_hash;
        let cache = unsafe { &mut *cache };
        println!(
            "process_qrinfo_from_message --: {:?} {:?} {:?}",
            processor, processor.opaque_context, cache
        );
        let offset = &mut 0;
        let read_list_diff =
            |offset: &mut usize| processor.read_list_diff_from_message(message, offset);
        let mut process_list_diff = |list_diff: llmq::MNListDiff| {
            processor.get_list_diff_result_internal_with_base_lookup(list_diff, cache)
        };
        let read_snapshot = |offset: &mut usize| llmq::LLMQSnapshot::from_bytes(message, offset);
        let read_var_int = |offset: &mut usize| encode::VarInt::from_bytes(message, offset);
        let snapshot_at_h_c = unwrap_or_qr_processing_failure!(read_snapshot(offset));
        let snapshot_at_h_2c = unwrap_or_qr_processing_failure!(read_snapshot(offset));
        let snapshot_at_h_3c = unwrap_or_qr_processing_failure!(read_snapshot(offset));
        let diff_tip = unwrap_or_qr_processing_failure!(read_list_diff(offset));
        let diff_h = unwrap_or_qr_processing_failure!(read_list_diff(offset));
        let diff_h_c = unwrap_or_qr_processing_failure!(read_list_diff(offset));
        let diff_h_2c = unwrap_or_qr_processing_failure!(read_list_diff(offset));
        let diff_h_3c = unwrap_or_qr_processing_failure!(read_list_diff(offset));
        let extra_share = message.read_with::<bool>(offset, {}).unwrap_or(false);
        let (snapshot_at_h_4c, diff_h_4c) = if extra_share {
            (
                Some(unwrap_or_qr_processing_failure!(read_snapshot(offset))),
                Some(unwrap_or_qr_processing_failure!(read_list_diff(offset))),
            )
        } else {
            (None, None)
        };
        processor.save_snapshot(diff_h_c.block_hash, snapshot_at_h_c.clone());
        processor.save_snapshot(diff_h_2c.block_hash, snapshot_at_h_2c.clone());
        processor.save_snapshot(diff_h_3c.block_hash, snapshot_at_h_3c.clone());
        if extra_share {
            processor.save_snapshot(
                diff_h_4c.as_ref().unwrap().block_hash,
                snapshot_at_h_4c.as_ref().unwrap().clone(),
            );
        }
        let last_quorum_per_index_count =
            unwrap_or_qr_processing_failure!(read_var_int(offset)).0 as usize;
        let mut last_quorum_per_index: Vec<masternode::LLMQEntry> =
            Vec::with_capacity(last_quorum_per_index_count);
        for _i in 0..last_quorum_per_index_count {
            let entry = unwrap_or_qr_processing_failure!(masternode::LLMQEntry::from_bytes(
                message, offset
            ));
            last_quorum_per_index.push(entry);
        }
        let quorum_snapshot_list_count =
            unwrap_or_qr_processing_failure!(read_var_int(offset)).0 as usize;
        let mut quorum_snapshot_list: Vec<llmq::LLMQSnapshot> =
            Vec::with_capacity(quorum_snapshot_list_count);
        for _i in 0..quorum_snapshot_list_count {
            quorum_snapshot_list.push(unwrap_or_qr_processing_failure!(read_snapshot(offset)));
        }
        let mn_list_diff_list_count =
            unwrap_or_qr_processing_failure!(read_var_int(offset)).0 as usize;
        let mut mn_list_diff_list: Vec<MNListDiffResult> =
            Vec::with_capacity(mn_list_diff_list_count);
        for _i in 0..mn_list_diff_list_count {
            mn_list_diff_list.push(process_list_diff(unwrap_or_qr_processing_failure!(
                read_list_diff(offset)
            )));
        }
        // The order is important since the each new one dependent on previous
        let result_at_h_4c = if let Some(diff) = diff_h_4c {
            Some(process_list_diff(diff))
        } else {
            None
        };
        let result_at_h_3c = process_list_diff(diff_h_3c);
        let result_at_h_2c = process_list_diff(diff_h_2c);
        let result_at_h_c = process_list_diff(diff_h_c);
        let result_at_h = process_list_diff(diff_h);
        let result_at_tip = process_list_diff(diff_tip);
        QRInfoResult {
            error_status: ProcessingError::None,
            result_at_tip,
            result_at_h,
            result_at_h_c,
            result_at_h_2c,
            result_at_h_3c,
            result_at_h_4c,
            snapshot_at_h_c,
            snapshot_at_h_2c,
            snapshot_at_h_3c,
            snapshot_at_h_4c,
            extra_share,
            last_quorum_per_index,
            quorum_snapshot_list,
            mn_list_diff_list,
        }
    }

    pub fn get_file_as_byte_vec(filename: &String) -> Vec<u8> {
        //println!("get_file_as_byte_vec: {}", filename);
        let mut f = fs::File::open(&filename).expect("no file found");
        let metadata = fs::metadata(&filename).expect("unable to read metadata");
        let mut buffer = vec![0; metadata.len() as usize];
        f.read(&mut buffer).expect("buffer overflow");
        buffer
    }

    pub fn message_from_file(name: String) -> Vec<u8> {
        let executable = env::current_exe().unwrap();
        let path = match executable.parent() {
            Some(name) => name,
            _ => panic!(),
        };
        let filepath = format!("{}/../../../files/{}", path.display(), name.as_str());
        println!("{:?}", filepath);
        let file = get_file_as_byte_vec(&filepath);
        file
    }

    pub fn assert_diff_result(context: &mut FFIContext, result: types::MNListDiffResult) {
        let masternode_list = unsafe { (*result.masternode_list).decode() };
        print!("block_hash: {} ({})", masternode_list.block_hash, masternode_list.block_hash.clone().reversed());
        let bh = context.block_for_hash(masternode_list.block_hash).unwrap().height;
        assert!(
            result.has_found_coinbase,
            "Did not find coinbase at height {}",
            bh
        );
        //turned off on purpose as we don't have the coinbase block
        //assert!(result.has_valid_coinbase, "Coinbase not valid at height {}", bh);
        assert!(
            result.has_valid_mn_list_root,
            "rootMNListValid not valid at height {}",
            bh
        );
        assert!(
            result.has_valid_llmq_list_root,
            "rootQuorumListValid not valid at height {}",
            bh
        );
        assert!(
            result.has_valid_quorums,
            "validQuorums not valid at height {}",
            bh
        );
    }

    pub unsafe extern "C" fn get_block_height_by_hash_from_context(
        block_hash: *mut [u8; 32],
        context: *const std::ffi::c_void,
    ) -> u32 {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let block_hash = UInt256(*block_hash);
        let block_hash_reversed = block_hash.clone().reversed();
        let block = data.block_for_hash(block_hash).unwrap_or(&MerkleBlock { hash: UInt256::MIN, height: u32::MAX, merkleroot: UInt256::MIN });
        let height = block.height;
        // println!("get_block_height_by_hash_from_context {}: {} ({})", height, block_hash_reversed, block_hash);
        height
    }

    pub unsafe extern "C" fn get_block_hash_by_height_default(
        _block_height: u32,
        _context: *const std::ffi::c_void,
    ) -> *mut u8 {
        null_mut()
    }

    pub unsafe extern "C" fn get_block_hash_by_height_from_context(
        block_height: u32,
        context: *const std::ffi::c_void,
    ) -> *mut u8 {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        if let Some(block) = data.block_for_height(block_height) {
            let block_hash = block.hash;
            println!("get_block_hash_by_height_from_context: {}: {:?}", block_height, block_hash.clone().reversed());
            block_hash.clone().0.as_mut_ptr()
            // block.hash.clone().reversed().0.as_mut_ptr()
        } else {
            null_mut()
        }
    }

    pub unsafe extern "C" fn get_llmq_snapshot_by_block_height_default(
        _block_height: u32,
        _context: *const std::ffi::c_void,
    ) -> *mut types::LLMQSnapshot {
        null_mut()
    }

    pub unsafe extern "C" fn get_llmq_snapshot_by_block_hash_default(
        _block_hash: *mut [u8; 32],
        _context: *const std::ffi::c_void,
    ) -> *mut types::LLMQSnapshot {
        null_mut()
    }

    pub unsafe extern "C" fn get_llmq_snapshot_by_block_hash_from_context(
        block_hash: *mut [u8; 32],
        context: *const std::ffi::c_void,
    ) -> *mut types::LLMQSnapshot {
        let h = UInt256(*(block_hash));
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        if let Some(snapshot) = data.cache.llmq_snapshots.get(&h) {
            println!("get_llmq_snapshot_by_block_hash_from_context: {}: {:?}", h, snapshot);
            boxed(snapshot.encode())
        } else {
            null_mut()
        }
    }

    pub unsafe extern "C" fn get_masternode_list_by_block_hash_default(
        _block_hash: *mut [u8; 32],
        _context: *const std::ffi::c_void,
    ) -> *mut types::MasternodeList {
        null_mut()
    }

    pub unsafe extern "C" fn get_masternode_list_by_block_hash_from_cache(
        block_hash: *mut [u8; 32],
        context: *const std::ffi::c_void,
    ) -> *mut types::MasternodeList {
        let h = UInt256(*(block_hash));
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        if let Some(list) = data.cache.mn_lists.get(&h) {
            println!("get_masternode_list_by_block_hash_from_cache: {}: masternodes: {} quorums: {} mn_merkle_root: {:?}, llmq_merkle_root: {:?}", h, list.masternodes.len(), list.quorums.len(), list.masternode_merkle_root, list.llmq_merkle_root);
            let encoded = list.encode();
            // &encoded as *const types::MasternodeList
            boxed(encoded)
        } else {
            null_mut()
        }
    }

    pub unsafe extern "C" fn masternode_list_save_default(
        _block_hash: *mut [u8; 32],
        _masternode_list: *mut types::MasternodeList,
        _context: *const std::ffi::c_void,
    ) -> bool {
        true
    }
    pub unsafe extern "C" fn masternode_list_save_in_cache(
        block_hash: *mut [u8; 32],
        masternode_list: *mut types::MasternodeList,
        context: *const std::ffi::c_void,
    ) -> bool {
        let h = UInt256(*(block_hash));
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let masternode_list = *masternode_list;
        let masternode_list_decoded = masternode_list.decode();
        println!("masternode_list_save_in_cache: {}", h);
        data.cache.mn_lists.insert(h, masternode_list_decoded);
        true
    }

    pub unsafe extern "C" fn masternode_list_destroy_default(
        _masternode_list: *mut types::MasternodeList,
    ) {
    }
    pub unsafe extern "C" fn hash_destroy_default(_hash: *mut u8) {}

    pub unsafe extern "C" fn should_process_diff_with_range_default(
        base_block_hash: *mut [u8; 32],
        block_hash: *mut [u8; 32],
        context: *const std::ffi::c_void,
    ) -> u8 {
        ProcessingError::None.into()
    }
    pub unsafe extern "C" fn snapshot_destroy_default(_snapshot: *mut types::LLMQSnapshot) {}
    pub unsafe extern "C" fn add_insight_lookup_default(
        _hash: *mut [u8; 32],
        _context: *const std::ffi::c_void,
    ) {
    }
    pub unsafe extern "C" fn save_llmq_snapshot_default(
        block_hash: *mut [u8; 32],
        snapshot: *mut types::LLMQSnapshot,
        _context: *const std::ffi::c_void,
    ) -> bool {
        true
    }
    pub unsafe extern "C" fn save_llmq_snapshot_in_cache(
        block_hash: *mut [u8; 32],
        snapshot: *mut types::LLMQSnapshot,
        context: *const std::ffi::c_void,
    ) -> bool {
        let h = UInt256(*(block_hash));
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let snapshot = *snapshot;
        let snapshot_decoded = snapshot.decode();
        data.cache.llmq_snapshots.insert(h, snapshot_decoded);
        true
    }

    pub unsafe extern "C" fn log_default(
        message: *const libc::c_char,
        _context: *const std::ffi::c_void,
    ) {
        let c_str = std::ffi::CStr::from_ptr(message);
        println!("{:?}", c_str.to_str().unwrap());
    }

    pub unsafe extern "C" fn get_merkle_root_by_hash_default(
        block_hash: *mut [u8; 32],
        context: *const std::ffi::c_void,
    ) -> *mut u8 {
        let block_hash = UInt256(*block_hash);
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let block_hash_reversed = block_hash.clone().reversed().0.to_hex();
        let mut merkle_root = if let Some(block) = data.block_for_hash(block_hash) {
            block.merkleroot
        } else {
            UInt256::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap()
        };
        println!("get_merkle_root_by_hash_default {} ({}) => ({})", block_hash, block_hash_reversed, merkle_root);
        merkle_root.0.as_mut_ptr()
    }

    pub unsafe extern "C" fn should_process_llmq_of_type(
        llmq_type: u8,
        context: *const std::ffi::c_void,
    ) -> bool {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);

        let quorum_type: u8 = match data.chain {
            ChainType::MainNet => LLMQType::Llmqtype400_60.into(),
            ChainType::TestNet => LLMQType::Llmqtype50_60.into(),
            ChainType::DevNet(_) => LLMQType::LlmqtypeDevnetDIP0024.into(),
        };
        llmq_type == quorum_type
    }
    pub unsafe extern "C" fn validate_llmq_callback(
        data: *mut types::LLMQValidationData,
        _context: *const std::ffi::c_void,
    ) -> bool {
        let result = unbox_any(data);
        let types::LLMQValidationData {
            items,
            count,
            commitment_hash,
            all_commitment_aggregated_signature,
            threshold_signature,
            public_key,
        } = *result;
        println!(
            "validate_quorum_callback: {:?}, {}, {:?}, {:?}, {:?}, {:?}",
            items,
            count,
            commitment_hash,
            all_commitment_aggregated_signature,
            threshold_signature,
            public_key
        );

        let all_commitment_aggregated_signature = UInt768(*all_commitment_aggregated_signature);
        let threshold_signature = UInt768(*threshold_signature);
        let public_key = UInt384(*public_key);
        let commitment_hash = UInt256(*commitment_hash);

        let infos = (0..count)
            .into_iter()
            .map(|i| AggregationInfo {
                public_key: UInt384(*(*(items.offset(i as isize)))),
                digest: commitment_hash,
            })
            .collect::<Vec<AggregationInfo>>();

        true
    }

    pub unsafe extern "C" fn get_block_hash_by_height_from_insight(block_height: u32, context: *const std::ffi::c_void) -> *mut u8 {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        match data.blocks.iter().find(|block| block.height == block_height) {
            Some(block) => block.hash.clone().0.as_mut_ptr(),
            None => match get_block_from_insight_by_height(block_height) {
                Some(block) => {
                    data.blocks.push(block.clone());
                    block.hash.clone().0.as_mut_ptr()
                },
                None => null_mut()
            }
        }
    }

    pub unsafe extern "C" fn get_block_height_by_hash_from_insight(block_hash: *mut [u8; 32], context: *const std::ffi::c_void) -> u32 {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let hash = UInt256(*block_hash);
        match data.blocks.iter().find(|block| block.hash == hash) {
            Some(block) => block.height,
            None => match get_block_from_insight_by_hash(hash) {
                Some(block) => {
                    data.blocks.push(block.clone());
                    block.height
                }
                None => u32::MAX
            }
        }
    }

    pub unsafe extern "C" fn get_merkle_root_by_hash_from_insight(block_hash: *mut [u8; 32], context: *const std::ffi::c_void) -> *mut u8 {
        let data: &mut FFIContext = &mut *(context as *mut FFIContext);
        let hash = UInt256(*block_hash);
        match data.blocks.iter().find(|block| block.hash == hash) {
            Some(block) => block.merkleroot.clone().0.as_mut_ptr(),
            None => match get_block_from_insight_by_hash(hash) {
                Some(block) => {
                    data.blocks.push(block);
                    block.merkleroot.clone().0.as_mut_ptr()
                },
                None => UInt256::MIN.clone().0.as_mut_ptr()
            }
        }
    }

    pub fn perform_mnlist_diff_test_for_message(
        hex_string: &str,
        should_be_total_transactions: u32,
        verify_string_hashes: Vec<&str>,
        verify_string_smle_hashes: Vec<&str>,
    ) {
        let bytes = Vec::from_hex(&hex_string).unwrap();
        let length = bytes.len();
        let c_array = bytes.as_ptr();
        let message: &[u8] = unsafe { slice::from_raw_parts(c_array, length) };
        let chain = ChainType::TestNet;
        let offset = &mut 0;
        assert!(length - *offset >= 32);
        let base_block_hash = UInt256::from_bytes(message, offset).unwrap();
        assert_ne!(
            base_block_hash,
            UInt256::default(), /*UINT256_ZERO*/
            "Base block hash should NOT be empty here"
        );
        assert!(length - *offset >= 32);
        let _block_hash = UInt256::from_bytes(message, offset).unwrap();
        assert!(length - *offset >= 4);
        let total_transactions = u32::from_bytes(message, offset).unwrap();
        assert_eq!(
            total_transactions, should_be_total_transactions,
            "Invalid transaction count"
        );
        let use_insight_as_backup = false;
        let base_masternode_list_hash: *const u8 = null_mut();
        let context = &mut FFIContext {
            chain,
            cache: &mut MasternodeProcessorCache::default(),
            blocks: init_testnet_store()
        } as *mut _ as *mut std::ffi::c_void;

        let cache = unsafe { processor_create_cache() };
        let processor = unsafe {
            register_processor(
                get_merkle_root_by_hash_default,
                get_block_height_by_hash_from_context,
                get_block_hash_by_height_default,
                get_llmq_snapshot_by_block_hash_default,
                save_llmq_snapshot_default,
                get_masternode_list_by_block_hash_default,
                masternode_list_save_default,
                masternode_list_destroy_default,
                add_insight_lookup_default,
                should_process_llmq_of_type,
                validate_llmq_callback,
                hash_destroy_default,
                snapshot_destroy_default,
                should_process_diff_with_range_default,
                log_default,
            )
        };

        let result = process_mnlistdiff_from_message(
            c_array,
            length,
            use_insight_as_backup,
            false,
            chain.genesis_hash().0.as_ptr(),
            processor,
            cache,
            context,
        );
        println!("result: {:?}", result);
        let result = unsafe { unbox_any(result) };
        let masternode_list = unsafe { (*unbox_any(result.masternode_list)).decode() };
        let masternodes = masternode_list.masternodes.clone();
        let mut pro_tx_hashes: Vec<UInt256> = masternodes.clone().into_keys().collect();
        pro_tx_hashes.sort();
        let mut verify_hashes: Vec<UInt256> = verify_string_hashes
            .into_iter()
            .map(|h| {
                Vec::from_hex(h)
                    .unwrap()
                    .read_with::<UInt256>(&mut 0, byte::LE)
                    .unwrap()
                    .reversed()
            })
            .collect();
        verify_hashes.sort();
        assert_eq!(verify_hashes, pro_tx_hashes, "Provider transaction hashes");
        let mut masternode_list_hashes: Vec<UInt256> = pro_tx_hashes
            .clone()
            .iter()
            .map(|hash| masternodes[hash].entry_hash)
            .collect();
        masternode_list_hashes.sort();
        let mut verify_smle_hashes: Vec<UInt256> = verify_string_smle_hashes
            .into_iter()
            .map(|h| {
                Vec::from_hex(h)
                    .unwrap()
                    .read_with::<UInt256>(&mut 0, byte::LE)
                    .unwrap()
            })
            .collect();
        verify_smle_hashes.sort();
        assert_eq!(
            masternode_list_hashes, verify_smle_hashes,
            "SMLE transaction hashes"
        );
        assert!(
            result.has_found_coinbase,
            "The coinbase was not part of provided hashes"
        );
    }
}
