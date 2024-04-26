use ckb_sdk::{constants::TYPE_ID_CODE_HASH, traits::CellQueryOptions};
use ckb_types::{
    core::ScriptHashType,
    packed::{OutPoint, Script},
    prelude::{Builder, Entity, Pack},
    H256,
};
use spore_types::generated::spore::{ClusterData, SporeData};

use crate::{
    client::RpcClient,
    types::{ClusterDescriptionField, DecoderLocationType, Error, Settings, SporeContentField},
};

type DecodeResult<T> = Result<T, Error>;

pub struct DOBDecoder {
    rpc: RpcClient,
    settings: Settings,
}

impl DOBDecoder {
    pub fn new(settings: Settings) -> Self {
        Self {
            rpc: RpcClient::new(&settings.ckb_rpc, &settings.ckb_rpc),
            settings,
        }
    }

    pub fn protocol_version(&self) -> String {
        self.settings.protocol_version.clone()
    }

    pub fn setting(&self) -> &Settings {
        &self.settings
    }

    pub async fn fetch_decode_ingredients(
        &self,
        spore_id: [u8; 32],
    ) -> DecodeResult<(SporeContentField, ClusterDescriptionField)> {
        let (dob_content, cluster_id) = self.fetch_dob_content(spore_id).await?;
        let dob_metadata = self.fetch_dob_metadata(cluster_id).await?;
        Ok((dob_content, dob_metadata))
    }

    // decode DNA under target spore_id
    pub async fn decode_dna(
        &self,
        dob_content: &SporeContentField,
        dob_metadata: ClusterDescriptionField,
    ) -> DecodeResult<String> {
        let dna = hex::decode(&dob_content.dna).map_err(|_| Error::HexedDNAParseError)?;
        if dna.len() != dob_metadata.dob.dna_bytes as usize {
            return Err(Error::DnaLengthNotMatch);
        }
        let decoder_path = match dob_metadata.dob.decoder.location {
            DecoderLocationType::CodeHash => {
                let mut decoder_path = self.settings.decoders_cache_directory.clone();
                decoder_path.push(format!(
                    "code_hash_{}.bin",
                    hex::encode(&dob_metadata.dob.decoder.hash)
                ));
                if !decoder_path.exists() {
                    let onchain_decoder =
                        self.settings
                            .onchain_decoder_deployment
                            .iter()
                            .find_map(|deployment| {
                                if deployment.code_hash == dob_metadata.dob.decoder.hash {
                                    Some(self.fetch_decoder_binary_directly(
                                        deployment.tx_hash.clone(),
                                        deployment.out_index,
                                    ))
                                } else {
                                    None
                                }
                            });
                    let Some(decoder_binary) = onchain_decoder else {
                        return Err(Error::NativeDecoderNotFound);
                    };
                    let decoder_file_content = decoder_binary.await?;
                    if ckb_hash::blake2b_256(&decoder_file_content)
                        != dob_metadata.dob.decoder.hash.0
                    {
                        return Err(Error::DecoderBinaryHashInvalid);
                    }
                    std::fs::write(decoder_path.clone(), decoder_file_content)
                        .map_err(|_| Error::DecoderBinaryPathInvalid)?;
                }
                decoder_path
            }
            DecoderLocationType::TypeId => {
                let mut decoder_path = self.settings.decoders_cache_directory.clone();
                decoder_path.push(format!(
                    "type_id_{}.bin",
                    hex::encode(&dob_metadata.dob.decoder.hash)
                ));
                if !decoder_path.exists() {
                    let decoder_binary = self
                        .fetch_decoder_binary(dob_metadata.dob.decoder.hash.into())
                        .await?;
                    std::fs::write(decoder_path.clone(), decoder_binary)
                        .map_err(|_| Error::DecoderBinaryPathInvalid)?;
                }
                decoder_path
            }
        };
        let dna = &dob_content.dna;
        let pattern = &dob_metadata.dob.pattern;
        #[cfg(not(feature = "embeded_vm"))]
        let raw_render_result = self.execute_externally(decoder_path, dna, pattern)?;
        #[cfg(feature = "embeded_vm")]
        let raw_render_result = {
            let (exit_code, outputs) = crate::vm::execute_riscv_binary(
                &decoder_path.to_string_lossy(),
                vec![dna.clone().into(), pattern.clone().into()],
            )
            .map_err(|_| Error::DecoderExecutionError)?;
            #[cfg(feature = "render_debug")]
            {
                println!("-------- DECODE RESULT ({exit_code}) ---------");
                outputs.iter().for_each(|output| println!("{output}"));
                println!("-------- DECODE RESULT END ---------");
            }
            if exit_code != 0 {
                return Err(Error::DecoderExecutionInternalError);
            }
            outputs.first().ok_or(Error::DecoderOutputInvalid)?.clone()
        };
        Ok(raw_render_result)
    }

    // invoke `ckb-vm-runner` in native machine and collect console output as result
    #[cfg(not(feature = "embeded_vm"))]
    fn execute_externally(
        &self,
        decoder_path: std::path::PathBuf,
        dna: &str,
        pattern: &str,
    ) -> DecodeResult<String> {
        let output = std::process::Command::new(&self.settings.ckb_vm_runner)
            .arg(decoder_path)
            .arg(dna)
            .arg(pattern)
            .output()
            .map_err(|_| Error::DecoderExecutionError)?;
        let raw_render_result = {
            let console_output = String::from_utf8_lossy(&output.stdout)
                .to_string()
                .replace('\\', "");
            let lines = console_output
                .split('\n')
                .map(|line| line.trim_matches('\"'))
                .collect::<Vec<_>>();
            #[cfg(feature = "render_debug")]
            {
                println!("-------- DECODE RESULT ---------");
                lines.iter().for_each(|line| println!("{line}"));
                println!("-------- DECODE RESULT END ---------");
            }
            lines
                .first()
                .ok_or(Error::DecoderOutputInvalid)?
                .to_string()
        };
        Ok(raw_render_result)
    }

    // search on-chain spore cell and return its content field, which represents dob content
    async fn fetch_dob_content(
        &self,
        spore_id: [u8; 32],
    ) -> DecodeResult<(SporeContentField, [u8; 32])> {
        let mut spore_cell = None;
        for spore_search_option in
            build_batch_search_options(spore_id, &self.settings.avaliable_spore_code_hashes)
        {
            spore_cell = self
                .rpc
                .get_cells(spore_search_option.into(), 1, None)
                .await
                .map_err(|_| Error::FetchLiveCellsError)?
                .objects
                .first()
                .cloned();
            if spore_cell.is_some() {
                break;
            }
        }
        let Some(spore_cell) = spore_cell else {
            return Err(Error::SporeIdNotFound);
        };
        let molecule_spore_data =
            SporeData::from_compatible_slice(spore_cell.output_data.unwrap_or_default().as_bytes())
                .map_err(|_| Error::SporeDataUncompatible)?;
        let content_type =
            String::from_utf8(molecule_spore_data.content_type().raw_data().to_vec())
                .map_err(|_| Error::SporeDataContentTypeUncompatible)?;
        if !content_type
            .to_string()
            .starts_with(&self.settings.protocol_version)
        {
            return Err(Error::DOBVersionUnexpected);
        }
        let cluster_id = molecule_spore_data
            .cluster_id()
            .to_opt()
            .ok_or(Error::ClusterIdNotSet)?
            .raw_data();
        let dob_content = serde_json::from_slice(&molecule_spore_data.content().raw_data())
            .map_err(|_| Error::DOBContentUnexpected)?;
        Ok((dob_content, cluster_id.to_vec().try_into().unwrap()))
    }

    // search on-chain cluster cell and return its description field, which contains dob metadata
    async fn fetch_dob_metadata(
        &self,
        cluster_id: [u8; 32],
    ) -> DecodeResult<ClusterDescriptionField> {
        let mut cluster_cell = None;
        for cluster_search_option in
            build_batch_search_options(cluster_id, &self.settings.avaliable_cluster_code_hashes)
        {
            cluster_cell = self
                .rpc
                .get_cells(cluster_search_option.into(), 1, None)
                .await
                .map_err(|_| Error::FetchLiveCellsError)?
                .objects
                .first()
                .cloned();
            if cluster_cell.is_some() {
                break;
            }
        }
        let Some(cluster_cell) = cluster_cell else {
            return Err(Error::ClusterIdNotFound);
        };
        let molecule_cluster_data = ClusterData::from_compatible_slice(
            cluster_cell.output_data.unwrap_or_default().as_bytes(),
        )
        .map_err(|_| Error::ClusterDataUncompatible)?;
        let dob_metadata = serde_json::from_slice(&molecule_cluster_data.description().raw_data())
            .map_err(|_| Error::DOBMetadataUnexpected)?;
        Ok(dob_metadata)
    }

    // search on-chain decoder cell, deployed with type_id feature enabled
    async fn fetch_decoder_binary(&self, decoder_id: [u8; 32]) -> DecodeResult<Vec<u8>> {
        let decoder_search_option = build_type_id_search_option(decoder_id);
        let decoder_cell = self
            .rpc
            .get_cells(decoder_search_option.into(), 1, None)
            .await
            .map_err(|_| Error::FetchLiveCellsError)?
            .objects
            .first()
            .cloned()
            .ok_or(Error::DecoderIdNotFound)?;
        Ok(decoder_cell
            .output_data
            .unwrap_or_default()
            .as_bytes()
            .into())
    }

    // search on-chain decoder cell, directly by its tx_hash and out_index
    async fn fetch_decoder_binary_directly(
        &self,
        tx_hash: H256,
        out_index: u32,
    ) -> DecodeResult<Vec<u8>> {
        let decoder_cell = self
            .rpc
            .get_live_cell(&OutPoint::new(tx_hash.pack(), out_index).into(), true)
            .await
            .map_err(|_| Error::FetchTransactionError)?;
        let decoder_binary = decoder_cell
            .cell
            .ok_or(Error::NoOutputCellInTransaction)?
            .data
            .ok_or(Error::DecoderBinaryNotFoundInCell)?
            .content;
        Ok(decoder_binary.as_bytes().to_vec())
    }
}

fn build_type_id_search_option(type_id_args: [u8; 32]) -> CellQueryOptions {
    let type_script = Script::new_builder()
        .code_hash(TYPE_ID_CODE_HASH.0.pack())
        .hash_type(ScriptHashType::Type.into())
        .args(type_id_args.to_vec().pack())
        .build();
    CellQueryOptions::new_type(type_script)
}

fn build_batch_search_options(
    type_args: [u8; 32],
    avaliable_code_hashes: &[H256],
) -> Vec<CellQueryOptions> {
    avaliable_code_hashes
        .into_iter()
        .map(|code_hash| {
            let type_script = Script::new_builder()
                .code_hash(code_hash.0.pack())
                .hash_type(ScriptHashType::Data1.into())
                .args(type_args.to_vec().pack())
                .build();
            CellQueryOptions::new_type(type_script)
        })
        .collect()
}
