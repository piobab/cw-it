use anyhow::bail;
use cosmrs::{
    crypto::secp256k1::SigningKey,
    proto::cosmos::{base::abci::v1beta1::GasInfo},
};
use cosmwasm_std::{
    coin, Addr, BankMsg, Binary, Coin, CosmosMsg, Empty, QueryRequest, StakingMsg, WasmMsg,
};
use cw_multi_test::{BankKeeper, BankSudo, BasicAppBuilder, StargateKeeper, StargateQueryHandler};
use osmosis_std::types::{
    cosmos::{
        bank::v1beta1::MsgSend,
        staking::v1beta1::{MsgBeginRedelegate, MsgDelegate, MsgUndelegate},
    },
    cosmwasm::wasm::v1::{
        MsgClearAdmin, MsgExecuteContract, MsgInstantiateContract, MsgMigrateContract,
        MsgUpdateAdmin,
    },
};
use prost::Message;
use serde::de::DeserializeOwned;
use std::str::FromStr;
use test_tube::{
    Account, DecodeError, EncodeError, FeeSetting, Runner, RunnerError, RunnerResult,
    SigningAccount,
};

use crate::traits::{ContractType, WasmRunner};

use super::modules::BankModule;

pub struct MultiTestRunner<'a> {
    pub app: cw_multi_test::App,
    pub address_prefix: &'a str,
}

const BANK_MODULE: BankModule = BankModule(BankKeeper {});

impl<'a> MultiTestRunner<'a> {
    pub fn new(address_prefix: &'a str) -> Self {
        // Setup stargate keeper with bank module support
        let mut stargate_keeper = StargateKeeper::new();
        BANK_MODULE.register_queries(&mut stargate_keeper);

        // Construct app
        let app = BasicAppBuilder::<Empty, Empty>::new()
            .with_stargate(stargate_keeper)
            .build(|_, _, _| {});

        Self {
            app,
            address_prefix,
        }
    }

    // TODO: move to trait
    pub fn init_account(&self, initial_balance: &[Coin]) -> RunnerResult<SigningAccount> {
        // Create a random signing account
        let signing_key = SigningKey::random();
        let account = SigningAccount::new(
            self.address_prefix.to_string(),
            signing_key,
            FeeSetting::Auto {
                gas_price: coin(0, "coin"),
                gas_adjustment: 1.0,
            },
        );

        // Mint the initial balances to the account
        if !initial_balance.is_empty() {
            self.app
                .sudo(
                    BankSudo::Mint {
                        to_address: account.address(),
                        amount: initial_balance.to_vec(),
                    }
                    .into(),
                )
                .unwrap();
        }

        Ok(account)
    }
}

impl Runner<'_> for MultiTestRunner<'_> {
    fn execute_cosmos_msgs<S>(
        &self,
        msgs: &[cosmwasm_std::CosmosMsg],
        signer: &test_tube::SigningAccount,
    ) -> test_tube::RunnerExecuteResult<S>
    where
        S: prost::Message + Default,
    {
        let sender = Addr::unchecked(signer.address());

        // Execute messages with multi test app
        let app_responses = self
            .app
            .execute_multi(sender, msgs.to_vec())
            .map_err(|e| RunnerError::GenericError(e.to_string()))?;

        // Construct test_tube::ExecuteResponse from cw_multi_test::AppResponse
        let events = app_responses
            .iter()
            .flat_map(|r| r.events.clone())
            .collect();
        let tmp = app_responses
            .iter()
            .map(|r| r.data.clone())
            .filter(|d| d.is_some())
            .collect::<Vec<_>>();
        let last_data = tmp.last().unwrap_or(&None);
        let data = match last_data {
            Some(d) => S::decode(d.as_slice()).unwrap(),
            None => S::default(),
        };
        let raw_data = data.encode_to_vec();
        let runner_res = test_tube::ExecuteResponse {
            data,
            events,
            raw_data,
            gas_info: GasInfo {
                gas_wanted: 0,
                gas_used: 0,
            },
        };

        Ok(runner_res)
    }

    fn execute_multiple<M, R>(
        &self,
        msgs: &[(M, &str)],
        signer: &test_tube::SigningAccount,
    ) -> test_tube::RunnerExecuteResult<R>
    where
        M: prost::Message,
        R: prost::Message + Default,
    {
        let encoded_msgs = msgs
            .iter()
            .map(|(msg, type_url)| {
                let mut buf = Vec::new();
                M::encode(msg, &mut buf).map_err(EncodeError::ProtoEncodeError)?;

                Ok(cosmrs::Any {
                    type_url: type_url.to_string(),
                    value: buf,
                })
            })
            .collect::<Result<Vec<cosmrs::Any>, RunnerError>>()?;

        self.execute_multiple_raw(encoded_msgs, signer)
    }

    fn execute_multiple_raw<R>(
        &self,
        msgs: Vec<cosmrs::Any>,
        signer: &test_tube::SigningAccount,
    ) -> test_tube::RunnerExecuteResult<R>
    where
        R: prost::Message + Default,
    {
        let msgs = msgs
            .iter()
            .map(|msg| match msg.type_url.as_str() {
                // WasmMsg
                MsgExecuteContract::TYPE_URL => {
                    let msg = MsgExecuteContract::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Wasm(WasmMsg::Execute {
                        contract_addr: msg.contract,
                        msg: Binary(msg.msg),
                        funds: msg
                            .funds
                            .into_iter()
                            .map(|c| coin(u128::from_str(&c.amount).unwrap(), c.denom))
                            .collect(),
                    }))
                }
                MsgInstantiateContract::TYPE_URL => {
                    let msg = MsgInstantiateContract::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Wasm(WasmMsg::Instantiate {
                        code_id: msg.code_id,
                        admin: Some(msg.admin),
                        msg: Binary(msg.msg),
                        funds: msg
                            .funds
                            .into_iter()
                            .map(|c| coin(u128::from_str(&c.amount).unwrap(), c.denom))
                            .collect(),
                        label: msg.label,
                    }))
                }
                MsgMigrateContract::TYPE_URL => {
                    let msg = MsgMigrateContract::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Wasm(WasmMsg::Migrate {
                        contract_addr: msg.contract,
                        new_code_id: msg.code_id,
                        msg: Binary(msg.msg),
                    }))
                }
                MsgUpdateAdmin::TYPE_URL => {
                    let msg = MsgUpdateAdmin::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Wasm(WasmMsg::UpdateAdmin {
                        contract_addr: msg.contract,
                        admin: msg.new_admin,
                    }))
                }
                MsgClearAdmin::TYPE_URL => {
                    let msg = MsgClearAdmin::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Wasm(WasmMsg::ClearAdmin {
                        contract_addr: msg.contract,
                    }))
                }
                // BankMsg
                MsgSend::TYPE_URL => {
                    let msg = MsgSend::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    Ok(CosmosMsg::<Empty>::Bank(BankMsg::Send {
                        to_address: msg.to_address,
                        amount: msg
                            .amount
                            .into_iter()
                            .map(|c| coin(u128::from_str(&c.amount).unwrap(), c.denom))
                            .collect(),
                    }))
                }
                // StakingMsg
                MsgDelegate::TYPE_URL => {
                    let msg = MsgDelegate::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    let proto_coin = msg.amount.unwrap_or_default();
                    Ok(CosmosMsg::<Empty>::Staking(StakingMsg::Delegate {
                        validator: msg.validator_address,
                        amount: coin(
                            u128::from_str(&proto_coin.amount).unwrap(),
                            proto_coin.denom,
                        ),
                    }))
                }
                MsgUndelegate::TYPE_URL => {
                    let msg = MsgUndelegate::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    let proto_coin = msg.amount.unwrap_or_default();
                    Ok(CosmosMsg::<Empty>::Staking(StakingMsg::Undelegate {
                        validator: msg.validator_address,
                        amount: coin(
                            u128::from_str(&proto_coin.amount).unwrap(),
                            proto_coin.denom,
                        ),
                    }))
                }
                MsgBeginRedelegate::TYPE_URL => {
                    let msg = MsgBeginRedelegate::decode(msg.value.as_slice())
                        .map_err(DecodeError::ProtoDecodeError)?;
                    let proto_coin = msg.amount.unwrap_or_default();
                    Ok(CosmosMsg::<Empty>::Staking(StakingMsg::Redelegate {
                        src_validator: msg.validator_src_address,
                        dst_validator: msg.validator_dst_address,
                        amount: coin(
                            u128::from_str(&proto_coin.amount).unwrap(),
                            proto_coin.denom,
                        ),
                    }))
                }
                _ => {
                    // Else assume StargateMsg
                    Ok(CosmosMsg::<Empty>::Stargate {
                        type_url: msg.type_url.clone(),
                        value: msg.value.clone().into(),
                    })
                }
            })
            .collect::<Result<Vec<_>, RunnerError>>()?;

        self.execute_cosmos_msgs(&msgs, signer)
    }

    fn query<Q, R>(&self, path: &str, query: &Q) -> test_tube::RunnerResult<R>
    where
        Q: prost::Message,
        R: prost::Message + DeserializeOwned + Default,
    {
        let querier = self.app.wrap();

        querier
            .query::<R>(&QueryRequest::Stargate {
                path: path.to_string(),
                data: query.encode_to_vec().into(),
            })
            .map_err(|e| RunnerError::GenericError(e.to_string()))
    }
}

impl<'a> WasmRunner<'a> for MultiTestRunner<'a> {
    fn store_code(
        &self,
        code: ContractType,
        _signer: &SigningAccount,
    ) -> Result<u64, anyhow::Error> {
        match code {
            ContractType::MultiTestContract(contract) => Ok(self.app.store_code(contract)),
            ContractType::Artifact(_) => bail!("Artifact not supported for MultiTestRunner"),
        }
    }
}

#[cfg(test)]
mod tests {
    use cosmrs::proto::cosmos::bank::v1beta1::MsgSendResponse;
    use cosmwasm_std::{coin, to_binary, Event};
    
    use osmosis_std::types::{
        cosmos::bank::v1beta1::QueryAllBalancesRequest,
        cosmwasm::wasm::v1::MsgInstantiateContractResponse,
    };
    use test_tube::{Bank, Module};

    use crate::{artifact::Artifact, helpers::upload_wasm_file};

    use super::*;

    mod counter {
        use cosmwasm_schema::{cw_serde, QueryResponses};

        #[cw_serde]
        pub struct InstantiateMsg {
            pub count: i32,
        }

        #[cw_serde]
        pub enum ExecuteMsg {
            Increment {},
            Reset { count: i32 },
        }

        #[cw_serde]
        #[derive(QueryResponses)]
        pub enum QueryMsg {
            // GetCount returns the current count as a json-encoded number
            #[returns(GetCountResponse)]
            GetCount {},
        }

        // We define a custom struct for each query response
        #[cw_serde]
        pub struct GetCountResponse {
            pub count: i32,
        }

        pub const WASM_PATH: &str = "artifacts/counter.wasm";
    }

    mod test_contract {
        use std::fmt;

        use cosmwasm_schema::{cw_serde, schemars::JsonSchema};
        use cosmwasm_std::{
            Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError, SubMsg, WasmMsg,
        };
        use cw_multi_test::{Contract, ContractWrapper};

        #[cw_serde]
        pub struct EmptyMsg {}

        fn instantiate(
            _deps: DepsMut,
            _env: Env,
            _info: MessageInfo,
            _msg: EmptyMsg,
        ) -> Result<Response, StdError> {
            Ok(Response::default())
        }

        fn execute(
            _deps: DepsMut,
            _env: Env,
            _info: MessageInfo,
            msg: WasmMsg,
        ) -> Result<Response, StdError> {
            let message = SubMsg::new(msg);

            Ok(Response::new().add_submessage(message))
        }

        fn query(_deps: Deps, _env: Env, _msg: EmptyMsg) -> Result<Binary, StdError> {
            Err(StdError::generic_err(
                "query not implemented for the `test_contract` contract",
            ))
        }

        pub fn test_contract<C>() -> Box<dyn Contract<C>>
        where
            C: Clone + fmt::Debug + PartialEq + JsonSchema + 'static,
        {
            let contract = ContractWrapper::new_with_empty(execute, instantiate, query);
            Box::new(contract)
        }
    }

    #[test]
    fn upload_contract() {
        let contract = ContractType::MultiTestContract(test_contract::test_contract());

        let app = MultiTestRunner::new("osmo");
        let alice = app.init_account(&[coin(1000, "uosmo")]).unwrap();

        let code_id = app.store_code(contract, &alice).unwrap();

        assert_eq!(code_id, 1);
    }

    #[test]
    #[should_panic]
    // This test should panic because we are trying to upload a wasm contract to a MultiTestRunner
    // which does not support wasm contracts.
    fn wasm_instantiate_contract() {
        let app = MultiTestRunner::new("osmo");
        let alice = app.init_account(&[coin(1000, "uosmo")]).unwrap();

        let code_id = upload_wasm_file(
            &app,
            &alice,
            ContractType::Artifact(Artifact::Local(counter::WASM_PATH.to_string())),
        )
        .unwrap();

        let init_msg = counter::InstantiateMsg { count: 17 };
        let msgs = vec![cosmwasm_std::CosmosMsg::Wasm(
            cosmwasm_std::WasmMsg::Instantiate {
                code_id,
                msg: to_binary(&init_msg).unwrap(),
                funds: vec![],
                admin: Some(alice.address()),
                label: "counter".to_string(),
            },
        )];

        let res = app
            .execute_cosmos_msgs::<MsgInstantiateContractResponse>(&msgs, &alice)
            .unwrap();

        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].ty, "instantiate".to_string());
    }

    #[test]
    fn bank_send() {
        let app = MultiTestRunner::new("osmo");
        let alice = app.init_account(&[coin(1000, "uatom")]).unwrap();
        let bob = app.init_account(&[]).unwrap();

        let msgs = vec![cosmwasm_std::CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: bob.address(),
            amount: vec![cosmwasm_std::Coin {
                denom: "uatom".to_string(),
                amount: 100u128.into(),
            }],
        })];

        let res = app
            .execute_cosmos_msgs::<MsgSendResponse>(&msgs, &alice)
            .unwrap();

        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.events[0],
            Event::new("transfer")
                .add_attribute("recipient", bob.address())
                .add_attribute("sender", alice.address())
                .add_attribute("amount", "100uatom")
        );
    }

    #[test]
    fn query_bank_through_test_tube_bank_module() {
        let app = MultiTestRunner::new("osmo");
        let alice = app.init_account(&[coin(1000, "uatom")]).unwrap();

        let bank = Bank::new(&app);

        let res = bank
            .query_all_balances(&QueryAllBalancesRequest {
                address: alice.address(),
                pagination: None,
            })
            .unwrap();

        assert_eq!(res.balances.len(), 1);
        assert_eq!(res.balances[0].denom, "uatom".to_string());
        assert_eq!(res.balances[0].amount, "1000");
    }
}
