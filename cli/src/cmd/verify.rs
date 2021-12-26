//! Verify contract source on etherscan

use crate::opts::forge::ContractInfo;
use crate::{cmd::Cmd, utils};
use cast::SimpleCast;
use ethers::{
    abi::{Address, Contract, Function},
    core::types::Chain,
    etherscan::{contract::VerifyContract, Client},
    prelude::{
        artifacts::{BytecodeObject, Source, Sources},
        Middleware, MinimalCombinedArtifacts, Project, ProjectCompileOutput, Provider,
    },
    solc::cache::SolFilesCache,
};
use http::Response;
use std::convert::TryFrom;
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(Debug, Clone, StructOpt)]
pub struct VerifyArgs {
    #[structopt(help = "contract source info `<path>:<contractname>` or `<contractname>`")]
    contract: ContractInfo,

    #[structopt(help = "deployed contract `address`")]
    address: Address,

    #[structopt(help = "constructor args for contract")]
    args: Vec<String>,
}

impl Cmd for VerifyArgs {
    fn run(self) -> eyre::Result<()> {
        let etherscan_api_key = utils::etherscan_api_key()?;
        let rt = tokio::runtime::Runtime::new().expect("could not start tokio rt");
        let chain = rt.block_on(self.get_chain());
        let project = self.opts.project()?;
        println!("compiling...");
        let compiled = project.compile()?;

        let (abi, _) = match self.contract.path {
            Some(ref path) => self.get_artifact_from_path(&project, path.clone())?,
            None => self.get_artifact_from_name(compiled)?,
        };

        let mut constructor_args = None;
        if let Some(constructor) = abi.unwrap().constructor {
            // convert constructor into function
            #[allow(deprecated)]
            let fun = Function {
                name: "constructor".to_string(),
                inputs: constructor.inputs,
                outputs: vec![],
                constant: false,
                state_mutability: Default::default(),
            };

            constructor_args = Some(SimpleCast::calldata(fun.abi_signature(), &self.args)?);
        } else if !self.args.is_empty() {
            eyre::bail!("No constructor found but contract arguments provided")
        }

        let chain = match chain {
            1 => Chain::Mainnet,
            3 => Chain::Ropsten,
            4 => Chain::Rinkeby,
            5 => Chain::Goerli,
            42 => Chain::Kovan,
            100 => Chain::XDai,
            _ => eyre::bail!("unexpected chain {}", chain),
        };
        let etherscan = Client::new(chain, etherscan_api_key)
            .map_err(|err| eyre::eyre!("Failed to create etherscan client: {}", err))?;

        let contract =
            VerifyContract::new(self.address, self.contract.path, self.get_compiler_version())
                .constructor_arguments(constructor_args);

        let resp = rt.block_on(self.submit(contract, etherscan));
        if resp.status == "0" {
            if resp.message == "Contract source code already verified" {
                println!("Contract source code already verified.");
                Ok(())
            } else {
                eyre::bail!(
                    "Encountered an error verifying this contract:\nResponse: `{}`\nDetails: `{}`",
                    resp.message,
                    resp.result
                );
            }
        } else {
            println!(
                r#"Submitted contract for verification:
                Response: `{}`
                GUID: `{}`
                url: {}#code"#,
                resp.message,
                resp.result,
                etherscan.address_url(self.address)
            );
            Ok(())
        }
    }
}

impl VerifyArgs {
    async fn get_chain(self) -> Result<u64> {
        let rpc_url = utils::rpc_url();
        let provider = Provider::try_from(self.rpc_url)?;
        let chain = provider
            .get_chainid()
            .await
            .map_err(|err| {
                eyre::eyre!(
                    r#"Please make sure that you are running a local Ethereum node:
        For example, try running either `parity' or `geth --rpc'.
        You could also try connecting to an external Ethereum node:
        For example, try `export ETH_RPC_URL=https://mainnet.infura.io'.
        If you have an Infura API key, add it to the end of the URL.

        Error: {}"#,
                    err
                )
            })?
            .as_u64();
        Ok(chain)
    }

    async fn submit(contract: VerifyContract, etherscan: Client) -> Result<Response<String>> {
        etherscan
            .submit_contract_verification(&contract)
            .await
            .map_err(|err| eyre::eyre!("Failed to submit contract verification: {}", err))?;
    }

    // TODO: These are imported from CreateArgs in creat.rs need to link them up
    fn get_artifact_from_name(
        &self,
        compiled: ProjectCompileOutput<MinimalCombinedArtifacts>,
    ) -> Result<(Contract, BytecodeObject)> {
        let mut has_found_contract = false;
        let mut contract_artifact = None;

        for (name, artifact) in compiled.into_artifacts() {
            let artifact_contract_name = name.split(':').collect::<Vec<_>>()[1];

            if artifact_contract_name == self.contract.name {
                if has_found_contract {
                    eyre::bail!("contract with duplicate name. pass path")
                }
                has_found_contract = true;
                contract_artifact = Some(artifact);
            }
        }

        Ok(match contract_artifact {
            Some(artifact) => (
                artifact.abi.ok_or_else(|| {
                    eyre::Error::msg(format!("abi not found for {}", self.contract.name))
                })?,
                artifact.bin.ok_or_else(|| {
                    eyre::Error::msg(format!("bytecode not found for {}", self.contract.name))
                })?,
            ),
            None => {
                eyre::bail!("could not find artifact")
            }
        })
    }

    // TODO: These are imported from CreateArgs in creat.rs need to link them up
    fn get_artifact_from_path(
        &self,
        project: &Project,
        path: String,
    ) -> Result<(Contract, BytecodeObject)> {
        // Get sources from the requested location
        let abs_path = std::fs::canonicalize(PathBuf::from(path))?;
        let mut sources = Sources::new();
        sources.insert(abs_path.clone(), Source::read(&abs_path)?);

        // Get artifact from the contract name and sources
        let mut config = SolFilesCache::builder().insert_files(sources.clone(), None)?;
        config.files.entry(abs_path).and_modify(|f| f.artifacts = vec![self.contract.name.clone()]);

        let artifacts = config
            .read_artifacts::<MinimalCombinedArtifacts>(project.artifacts_path())?
            .into_values()
            .collect::<Vec<_>>();

        if artifacts.is_empty() {
            eyre::bail!("could not find artifact")
        } else if artifacts.len() > 1 {
            eyre::bail!("duplicate contract name in the same source file")
        }
        let artifact = artifacts[0].clone();

        Ok((
            artifact.abi.ok_or_else(|| {
                eyre::Error::msg(format!("abi not found for {}", self.contract.name))
            })?,
            artifact.bin.ok_or_else(|| {
                eyre::Error::msg(format!("bytecode not found for {}", self.contract.name))
            })?,
        ))
    }

    fn get_compiler_version(self) -> String {
        let contract_reader = std::io::BufReader::new(std::fs::File::open(self.contract.path)?);
        let compiler_line =
            contract_reader.lines().find(|line| line.unwrap().starts_with("pragma solidity"));
        compiler_line.split_whitespace().nth(2).unwrap();
    }
}
