use pallas_codec::utils::Bytes;
use pallas_primitives::conway::{CostModel, CostModels, Language, MintedTx, Redeemer, RedeemerTag};
use uplc::{
    ast::{FakeNamedDeBruijn, NamedDeBruijn, Program},
    machine::cost_model::ExBudget,
    tx::{
        script_context::{
            find_script, DataLookupTable, PlutusScript, ScriptContext, TxInfo, TxInfoV1, TxInfoV2,
            TxInfoV3,
        },
        to_plutus_data::ToPlutusData,
        ResolvedInput, SlotConfig,
    },
    PlutusData,
};

use crate::{
    common::ExUnits,
    validators::{
        common::NetworkType, phase_2::errors::Phase2Error, validation_result::EvalRedeemerResult,
    },
};

use crate::validators::validation_result::RedeemerTag as ValidatorRedeemerTag;

pub fn slot_config_network(network: &NetworkType) -> SlotConfig {
    match network {
        NetworkType::Mainnet => SlotConfig {
            zero_time: 1596059091000,
            zero_slot: 4492800,
            slot_length: 1000,
        },
        NetworkType::Preview => SlotConfig {
            zero_time: 1666656000000,
            zero_slot: 0,
            slot_length: 1000,
        },
        NetworkType::Preprod => SlotConfig {
            zero_time: 1654041600000 + 1728000000,
            zero_slot: 86400,
            slot_length: 1000,
        },
    }
}

pub fn eval_redeemer(
    tx: &MintedTx,
    utxos: &[ResolvedInput],
    slot_config: &SlotConfig,
    redeemer: &Redeemer,
    lookup_table: &DataLookupTable,
    cost_mdls_opt: Option<&CostModels>,
    initial_budget: &ExBudget,
) -> (EvalRedeemerResult, Option<Phase2Error>) {
    fn do_eval_redeemer(
        cost_mdl_opt: Option<&CostModel>,
        initial_budget: &ExBudget,
        lang: &Language,
        datum: Option<PlutusData>,
        redeemer: &Redeemer,
        tx_info: TxInfo,
        program: Program<NamedDeBruijn>,
    ) -> (EvalRedeemerResult, Option<Phase2Error>) {
        let script_context = tx_info
            .into_script_context(redeemer, datum.as_ref())
            .expect("couldn't create script context from transaction?");

        let program = match script_context {
            ScriptContext::V1V2 { .. } => if let Some(datum) = datum {
                program.apply_data(datum)
            } else {
                program
            }
            .apply_data(redeemer.data.clone())
            .apply_data(script_context.to_plutus_data()),

            ScriptContext::V3 { .. } => program.apply_data(script_context.to_plutus_data()),
        };

        let eval_result = if let Some(costs) = cost_mdl_opt {
            program.eval_as(lang, costs, Some(initial_budget))
        } else {
            program.eval_version(ExBudget::max(), lang)
        };

        let cost = eval_result.cost();
        let logs = eval_result.logs();

        let error = match eval_result.result() {
            Ok(_) => None,
            Err(err) => Some(Phase2Error::MachineError {
                error: err.to_string(),
            }),
        };

        let new_redeemer = EvalRedeemerResult {
            tag: map_tag_to_redeemer_tag(&redeemer.tag),
            index: redeemer.index as u64,
            calculated_ex_units: ExUnits {
                mem: cost.mem as u64,
                steps: cost.cpu as u64,
            },
            provided_ex_units: ExUnits {
                mem: redeemer.ex_units.mem,
                steps: redeemer.ex_units.steps,
            },
            success: error.as_ref().is_none(),
            error: error.as_ref().map(|e| e.to_string()),
            logs: logs,
        };

        (new_redeemer, error)
    }

    let program = |script: Bytes| {
        let mut buffer = Vec::new();
        Program::<FakeNamedDeBruijn>::from_cbor(&script, &mut buffer)
            .map(Into::<Program<NamedDeBruijn>>::into)
    };

    let redeemers_script = find_script(redeemer, tx, utxos, lookup_table).map_err(|e| {
        Phase2Error::MissingScriptForRedeemer {
            error: e.to_string(),
        }
    });

    (|| -> Result<(EvalRedeemerResult, Option<Phase2Error>), Phase2Error> {
        match redeemers_script {
            Ok((PlutusScript::V1(script), datum)) => Ok(do_eval_redeemer(
                cost_mdls_opt
                    .map(|cost_mdls| {
                        cost_mdls
                            .plutus_v1
                            .as_ref()
                            .ok_or(Phase2Error::CostModelNotFound {
                                language: language_to_string(&Language::PlutusV1),
                            })
                    })
                    .transpose()?,
                initial_budget,
                &Language::PlutusV1,
                datum,
                redeemer,
                TxInfoV1::from_transaction(tx, utxos, slot_config).map_err(|err| {
                    Phase2Error::BuildTxContextError {
                        error: err.to_string(),
                    }
                })?,
                program(Bytes::from(script.as_ref().to_vec())).map_err(|err| Phase2Error::ScriptDecodeError {
                    error: err.to_string(),
                })?,
            )),

            Ok((PlutusScript::V2(script), datum)) => Ok(do_eval_redeemer(
                cost_mdls_opt
                    .map(|cost_mdls| {
                        cost_mdls
                            .plutus_v2
                            .as_ref()
                            .ok_or(Phase2Error::CostModelNotFound {
                                language: language_to_string(&Language::PlutusV2),
                            })
                    })
                    .transpose()?,
                initial_budget,
                &Language::PlutusV2,
                datum,
                redeemer,
                TxInfoV2::from_transaction(tx, utxos, slot_config).map_err(|err| {
                    Phase2Error::BuildTxContextError {
                        error: err.to_string(),
                    }
                })?,
                program(Bytes::from(script.as_ref().to_vec())).map_err(|err| Phase2Error::ScriptDecodeError {
                    error: err.to_string(),
                })?,
            )),

            Ok((PlutusScript::V3(script), datum)) => Ok(do_eval_redeemer(
                cost_mdls_opt
                    .map(|cost_mdls| {
                        cost_mdls
                            .plutus_v3
                            .as_ref()
                            .ok_or(Phase2Error::CostModelNotFound {
                                language: language_to_string(&Language::PlutusV3),
                            })
                    })
                    .transpose()?,
                initial_budget,
                &Language::PlutusV3,
                datum,
                redeemer,
                TxInfoV3::from_transaction(tx, utxos, slot_config).map_err(|err| {
                    Phase2Error::BuildTxContextError {
                        error: err.to_string(),
                    }
                })?,
                program(Bytes::from(script.as_ref().to_vec())).map_err(|err| Phase2Error::ScriptDecodeError {
                    error: err.to_string(),
                })?,
            )),
            Err(e) => Err(e),
        }
    })()
    .unwrap_or_else(|e| eval_redeemer_result(redeemer, e))
}

fn eval_redeemer_result(
    redeemer: &Redeemer,
    error: Phase2Error,
) -> (EvalRedeemerResult, Option<Phase2Error>) {
    let new_redeemer = EvalRedeemerResult {
        tag: map_tag_to_redeemer_tag(&redeemer.tag),
        index: redeemer.index as u64,
        calculated_ex_units: ExUnits { mem: 0, steps: 0 },
        provided_ex_units: ExUnits {
            mem: redeemer.ex_units.mem,
            steps: redeemer.ex_units.steps,
        },
        success: false,
        error: Some(error.to_string()),
        logs: vec![],
    };
    (new_redeemer, Some(error))
}

fn language_to_string(language: &Language) -> String {
    match language {
        Language::PlutusV1 => "PlutusV1".to_string(),
        Language::PlutusV2 => "PlutusV2".to_string(),
        Language::PlutusV3 => "PlutusV3".to_string(),
    }
}

fn map_tag_to_redeemer_tag(tag: &RedeemerTag) -> ValidatorRedeemerTag {
    match tag {
        RedeemerTag::Mint => ValidatorRedeemerTag::Mint,
        RedeemerTag::Spend => ValidatorRedeemerTag::Spend,
        RedeemerTag::Cert => ValidatorRedeemerTag::Cert,
        RedeemerTag::Propose => ValidatorRedeemerTag::Propose,
        RedeemerTag::Vote => ValidatorRedeemerTag::Vote,
        RedeemerTag::Reward  => ValidatorRedeemerTag::Reward,
    }
}
