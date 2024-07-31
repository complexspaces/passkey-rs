use p256::SecretKey;
use passkey_types::{
    ctap2::{
        make_credential::{Request, Response},
        AttestedCredentialData, AuthenticatorData, Ctap2Error, StatusCode,
    },
    Passkey,
};

use crate::{Authenticator, CoseKeyPair, CredentialStore, UserValidationMethod};

impl<S, U> Authenticator<S, U>
where
    S: CredentialStore + Sync,
    U: UserValidationMethod + Sync,
{
    /// This method is invoked by the host to request generation of a new credential in the authenticator.
    pub async fn make_credential(&mut self, input: Request) -> Result<Response, StatusCode> {
        let flags = if input.options.up {
            self.check_user(&input.options, None).await?
        } else {
            return Err(Ctap2Error::InvalidOption.into());
        };

        // 1. If the excludeList parameter is present and contains a credential ID that is present
        //    on this authenticator and bound to the specified rpId, wait for user presence, then
        //    terminate this procedure and return error code CTAP2_ERR_CREDENTIAL_EXCLUDED. User
        //    presence check is required for CTAP2 authenticators before the RP gets told that the
        //    token is already registered to behave similarly to CTAP1/U2F authenticators.

        if input
            .exclude_list
            .as_ref()
            .filter(|list| !list.is_empty())
            .is_some()
        {
            if let Ok(false) = self
                .store()
                .find_credentials(input.exclude_list.as_deref(), &input.rp.id)
                .await
                .map(|creds| creds.is_empty())
            {
                return Err(Ctap2Error::CredentialExcluded.into());
            }
        }

        // 2. If the pubKeyCredParams parameter does not contain a valid COSEAlgorithmIdentifier
        //    value that is supported by the authenticator, terminate this procedure and return
        //    error code CTAP2_ERR_UNSUPPORTED_ALGORITHM.
        let algorithm = self.choose_algorithm(&input.pub_key_cred_params)?;

        // 3. If the options parameter is present, process all the options. If the option is known
        //    but not supported, terminate this procedure and return CTAP2_ERR_UNSUPPORTED_OPTION.
        //    If the option is known but not valid for this command, terminate this procedure and
        //    return CTAP2_ERR_INVALID_OPTION. Ignore any options that are not understood.
        //    Note that because this specification defines normative behaviors for them, all
        //    authenticators MUST understand the "rk", "up", and "uv" options.
        // NOTE: Some of this step is handled at the very begining of the method

        //    4. If the "rk" option is present then:
        //       1. If the rk option ID is not present in authenticatorGetInfo response, end the operation by returning CTAP2_ERR_UNSUPPORTED_OPTION.
        if input.options.rk && !self.get_info().await.options.unwrap_or_default().rk {
            return Err(Ctap2Error::UnsupportedOption.into());
        }

        // 4. TODO, if the extensions parameter is present, process any extensions that this
        //    authenticator supports. Authenticator extension outputs generated by the authenticator
        //    extension processing are returned in the authenticator data.

        // NB: We do not currently support any Pin Protocols (1 or 2) as this does not make sense
        // in the context of 1Password. This is to be revisited to see if we can hook this into
        // using some key that we already have, such as the Biometry unlock key for example.
        // 5. If pinAuth parameter is present and pinProtocol is 1, verify it by matching it against
        //    first 16 bytes of HMAC-SHA-256 of clientDataHash parameter using
        //    pinToken: HMAC- SHA-256(pinToken, clientDataHash).
        //     1. If the verification succeeds, set the "uv" bit to 1 in the response.
        //     2. If the verification fails, return CTAP2_ERR_PIN_AUTH_INVALID error.
        // 6. If pinAuth parameter is not present and clientPin been set on the authenticator,
        //    return CTAP2_ERR_PIN_REQUIRED error.
        // 7. If pinAuth parameter is present and the pinProtocol is not supported,
        //    return CTAP2_ERR_PIN_AUTH_INVALID.
        if input.pin_auth.is_some() {
            // we currently don't support pin authentication
            return Err(Ctap2Error::UnsupportedOption.into());
        }

        // 8. If the authenticator has a display, show the items contained within the user and rp
        //    parameter structures to the user. Alternatively, request user interaction in an
        //    authenticator-specific way (e.g., flash the LED light). Request permission to create
        //    a credential. If the user declines permission, return the CTAP2_ERR_OPERATION_DENIED
        //    error.

        // 9. Generate a new credential key pair for the algorithm specified.
        let credential_id: Vec<u8> = {
            use rand::RngCore;
            let mut data = vec![0u8; 16];
            rand::thread_rng().fill_bytes(&mut data);
            data
        };

        let private_key = {
            let mut rng = rand::thread_rng();
            SecretKey::random(&mut rng)
        };

        let extensions = self.make_extensions(input.extensions, input.options.uv)?;

        // Encoding of the key pair into their CoseKey representation before moving the private CoseKey
        // into the passkey. Keeping the public key ready for step 11 below and returning the attested
        // credential.
        let CoseKeyPair { public, private } = CoseKeyPair::from_secret_key(&private_key, algorithm);

        let passkey = Passkey {
            key: private,
            rp_id: input.rp.id.clone(),
            credential_id: credential_id.into(),
            user_handle: input.options.rk.then_some(input.user.id.clone()),
            counter: self.make_credentials_with_signature_counter.then_some(0),
            extensions: extensions.credential,
        };

        // 10. If "rk" in options parameter is set to true:
        //     1. If a credential for the same RP ID and account ID already exists on the
        //        authenticator, overwrite that credential.
        //     2. Store the user parameter along the newly-created key pair.
        //     3. If authenticator does not have enough internal storage to persist the new
        //        credential, return CTAP2_ERR_KEY_STORE_FULL.
        // --> This seems like in the wrong place since we still need the passkey, see after step 11.

        // 11. Generate an attestation statement for the newly-created key using clientDataHash.

        // SAFETY: the only case where this fails is if credential_id's length cannot be represented
        // as a u16. This is checked at step 9, therefore this will never return an error
        let acd = AttestedCredentialData::new(
            *self.aaguid(),
            passkey.credential_id.clone().into(),
            public,
        )
        .unwrap();

        let auth_data = AuthenticatorData::new(&input.rp.id, passkey.counter)
            .set_flags(flags)
            .set_attested_credential_data(acd)
            .set_make_credential_extensions(extensions.signed)?;

        let response = Response {
            fmt: "None".into(),
            auth_data,
            att_stmt: coset::cbor::value::Value::Map(vec![]),
            ep_att: None,
            large_blob_key: None,
            unsigned_extension_outputs: extensions.unsigned,
        };

        // 10
        self.store_mut()
            .save_credential(passkey, input.user.into(), input.rp, input.options)
            .await?;

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use coset::iana;
    use passkey_types::{
        ctap2::{
            extensions::{AuthenticatorPrfInputs, AuthenticatorPrfValues},
            make_credential::{
                ExtensionInputs, Options, PublicKeyCredentialRpEntity,
                PublicKeyCredentialUserEntity,
            },
            Aaguid,
        },
        rand::random_vec,
        webauthn, Bytes,
    };

    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        credential_store::{DiscoverabilitySupport, StoreInfo},
        extensions,
        user_validation::MockUserValidationMethod,
        MemoryStore,
    };

    fn good_request() -> Request {
        Request {
            client_data_hash: random_vec(32).into(),
            rp: PublicKeyCredentialRpEntity {
                id: "future.1password.com".into(),
                name: Some("1password".into()),
            },
            user: webauthn::PublicKeyCredentialUserEntity {
                id: random_vec(16).into(),
                display_name: "wendy".into(),
                name: "Appleseed".into(),
            },
            pub_key_cred_params: vec![webauthn::PublicKeyCredentialParameters {
                ty: webauthn::PublicKeyCredentialType::PublicKey,
                alg: iana::Algorithm::ES256,
            }],
            exclude_list: None,
            extensions: None,
            options: Options {
                rk: true,
                up: true,
                uv: true,
            },
            pin_auth: None,
            pin_protocol: None,
        }
    }

    #[tokio::test]
    async fn assert_storage_on_success() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock);

        let request = good_request();

        authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        let store = shared_store.lock().await;

        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn assert_excluded_credentials() {
        let cred_id: Bytes = random_vec(16).into();
        let response = Request {
            exclude_list: Some(vec![webauthn::PublicKeyCredentialDescriptor {
                ty: webauthn::PublicKeyCredentialType::PublicKey,
                id: cred_id.clone(),
                transports: Some(vec![webauthn::AuthenticatorTransport::Usb]),
            }]),
            ..good_request()
        };
        let passkey = Passkey {
            // contents of key doesn't matter, only the id
            key: Default::default(),
            rp_id: "".into(),
            credential_id: cred_id.clone(),
            user_handle: Some(response.user.id.clone()),
            counter: None,
            extensions: Default::default(),
        };
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        shared_store.lock().await.insert(cred_id.into(), passkey);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock);

        let err = authenticator
            .make_credential(response)
            .await
            .expect_err("make credential succeeded even though store contains excluded id");

        assert_eq!(err, Ctap2Error::CredentialExcluded.into());
        assert_eq!(shared_store.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn assert_unsupported_algorithm() {
        let user_mock = MockUserValidationMethod::verified_user(1);
        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), MemoryStore::new(), user_mock);

        let request = Request {
            pub_key_cred_params: vec![webauthn::PublicKeyCredentialParameters {
                ty: webauthn::PublicKeyCredentialType::PublicKey,
                alg: iana::Algorithm::RSAES_OAEP_SHA_256,
            }],
            ..good_request()
        };

        let err = authenticator
            .make_credential(request)
            .await
            .expect_err("Succeeded with an unsupported algorithm");

        assert_eq!(err, Ctap2Error::UnsupportedAlgorithm.into());
    }

    #[tokio::test]
    async fn make_credential_counter_is_some_0_when_counters_are_enabled() {
        // Arrange
        let shared_store = Arc::new(Mutex::new(None));
        let user_mock = MockUserValidationMethod::verified_user(1);
        let request = good_request();
        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock);
        authenticator.set_make_credentials_with_signature_counter(true);

        // Act
        authenticator.make_credential(request).await.unwrap();

        // Assert
        let store = shared_store.lock().await;
        assert_eq!(store.as_ref().and_then(|c| c.counter).unwrap(), 0);
    }

    #[tokio::test]
    async fn unsupported_extension_with_request_gives_no_ext_output() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock);

        let request = Request {
            extensions: Some(ExtensionInputs {
                prf: Some(AuthenticatorPrfInputs {
                    eval: None,
                    eval_by_credential: None,
                }),
                ..Default::default()
            }),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_none());
    }

    #[tokio::test]
    async fn unsupported_extension_with_empty_request_gives_no_ext_output() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);
        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock);

        let request = Request {
            extensions: Some(ExtensionInputs::default()),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_none());
    }

    #[tokio::test]
    async fn supported_extension_with_empty_request_gives_no_ext_output() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock)
                .hmac_secret(extensions::HmacSecretConfig::new_with_uv_only());

        let request = Request {
            extensions: Some(ExtensionInputs::default()),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_none());
    }

    #[tokio::test]
    async fn supported_extension_without_extension_request_gives_no_ext_output() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock)
                .hmac_secret(extensions::HmacSecretConfig::new_with_uv_only());

        let request = good_request();

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_none());
    }

    #[tokio::test]
    async fn supported_extension_with_request_gives_output() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock)
                .hmac_secret(extensions::HmacSecretConfig::new_with_uv_only());

        let request = Request {
            extensions: Some(ExtensionInputs {
                prf: Some(AuthenticatorPrfInputs {
                    eval: None,
                    eval_by_credential: None,
                }),
                ..Default::default()
            }),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_some());
        let exts = res.unsigned_extension_outputs.unwrap();
        assert!(exts.prf.is_some());
        let prf = exts.prf.unwrap();
        assert!(prf.enabled);
        assert!(prf.results.is_none())
    }

    #[tokio::test]
    async fn hmac_secret_mc_happy_path() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock).hmac_secret(
                extensions::HmacSecretConfig::new_with_uv_only().enable_on_make_credential(),
            );

        let request = Request {
            extensions: Some(ExtensionInputs {
                prf: Some(AuthenticatorPrfInputs {
                    eval: Some(AuthenticatorPrfValues {
                        first: random_vec(32).try_into().unwrap(),
                        second: Some(random_vec(32).try_into().unwrap()),
                    }),
                    eval_by_credential: None,
                }),
                ..Default::default()
            }),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());

        assert!(res.unsigned_extension_outputs.is_some());
        let exts = res.unsigned_extension_outputs.unwrap();

        assert!(exts.prf.is_some());
        let prf = exts.prf.unwrap();

        assert!(prf.enabled);
        assert!(prf.results.is_some());
        let values = prf.results.unwrap();

        assert!(!values.first.is_empty());
        // We expect this to be None because the authenticator requires UV.
        // When calculating hmac secrets, it will skip the second input if
        // the authenticator does not support "no UV".
        assert!(values.second.is_none());
    }

    #[tokio::test]
    async fn hmac_secret_mc_without_hmac_secret_support() {
        let shared_store = Arc::new(Mutex::new(MemoryStore::new()));
        let user_mock = MockUserValidationMethod::verified_user(1);

        let mut authenticator =
            Authenticator::new(Aaguid::new_empty(), shared_store.clone(), user_mock)
                //support on make credential is not set.
                .hmac_secret(extensions::HmacSecretConfig::new_with_uv_only());

        let request = Request {
            extensions: Some(ExtensionInputs {
                prf: Some(AuthenticatorPrfInputs {
                    eval: Some(AuthenticatorPrfValues {
                        first: random_vec(32).try_into().unwrap(),
                        second: None,
                    }),
                    eval_by_credential: None,
                }),
                ..Default::default()
            }),
            ..good_request()
        };

        let res = authenticator
            .make_credential(request)
            .await
            .expect("error happened while trying to make a new credential");

        assert!(res.auth_data.extensions.is_none());
        assert!(res.unsigned_extension_outputs.is_some());
        let exts = res.unsigned_extension_outputs.unwrap();
        assert!(exts.prf.is_some());
        let prf = exts.prf.unwrap();
        assert!(prf.enabled);
        assert!(prf.results.is_none())
    }

    #[tokio::test]
    async fn make_credential_returns_err_when_rk_is_requested_but_not_supported() {
        struct StoreWithoutDiscoverableSupport;
        #[async_trait::async_trait]
        impl CredentialStore for StoreWithoutDiscoverableSupport {
            type PasskeyItem = Passkey;

            async fn find_credentials(
                &self,
                _id: Option<&[webauthn::PublicKeyCredentialDescriptor]>,
                _rp_id: &str,
            ) -> Result<Vec<Self::PasskeyItem>, StatusCode> {
                #![allow(clippy::unimplemented)]
                unimplemented!("The test should not call find_credentials")
            }

            async fn save_credential(
                &mut self,
                _cred: Passkey,
                _user: PublicKeyCredentialUserEntity,
                _rp: PublicKeyCredentialRpEntity,
                _options: Options,
            ) -> Result<(), StatusCode> {
                #![allow(clippy::unimplemented)]
                unimplemented!("The test should not call save_credential")
            }

            async fn update_credential(&mut self, _cred: Passkey) -> Result<(), StatusCode> {
                #![allow(clippy::unimplemented)]
                unimplemented!("The test should not call update_credential")
            }

            async fn get_info(&self) -> StoreInfo {
                StoreInfo {
                    discoverability: DiscoverabilitySupport::OnlyNonDiscoverable,
                }
            }
        }

        // Arrange
        let store = StoreWithoutDiscoverableSupport;
        let user_mock = MockUserValidationMethod::verified_user(1);
        let request = good_request();
        let mut authenticator = Authenticator::new(Aaguid::new_empty(), store, user_mock);
        authenticator.set_make_credentials_with_signature_counter(true);

        // Act
        let err = authenticator
            .make_credential(request)
            .await
            .expect_err("Succeeded with unsupported rk");

        // Assert
        assert_eq!(err, Ctap2Error::UnsupportedOption.into());
    }
}
