use p256::ecdsa::{signature::SignerMut, SigningKey};
use passkey_types::{
    ctap2::{
        get_assertion::{Request, Response},
        AuthenticatorData, Ctap2Error, Flags, StatusCode,
    },
    webauthn::PublicKeyCredentialUserEntity,
    Passkey,
};

use crate::{private_key_from_cose_key, Authenticator, CredentialStore, UserValidationMethod};

impl<S: CredentialStore + Sync, U> Authenticator<S, U>
where
    S: CredentialStore + Sync,
    U: UserValidationMethod<PasskeyItem = <S as CredentialStore>::PasskeyItem> + Sync,
    Passkey: TryFrom<<S as CredentialStore>::PasskeyItem> + Clone,
{
    /// This method is used by a host to request cryptographic proof of user authentication as well
    /// as user consent to a given transaction, using a previously generated credential that is
    /// bound to the authenticator and relying party identifier.
    pub async fn get_assertion(&mut self, input: Request) -> Result<Response, StatusCode> {
        // 1. Locate all credentials that are eligible for retrieval under the specified criteria:
        //     1. If an allowList is present and is non-empty, locate all denoted credentials
        //        present on this authenticator and bound to the specified rpId.
        //     2. If an allowList is not present, locate all credentials that are present on this
        //        authenticator and bound to the specified rpId.
        //     3. Let numberOfCredentials be the number of credentials found.
        //        --> Seeing as we handle 1 credential per account for an RP, returning the number
        //            of credentials leaks the number of accounts that is stored. This is not ideal,
        //            therefore we will never populate this field.
        let maybe_credential = self
            .store()
            .find_credentials(
                input
                    .allow_list
                    .as_deref()
                    .filter(|inner| !inner.is_empty()),
                &input.rp_id,
            )
            .await
            .and_then(|c| c.into_iter().next().ok_or(Ctap2Error::NoCredentials.into()));

        // 2. If pinAuth parameter is present and pinProtocol is 1, verify it by matching it against
        //    first 16 bytes of HMAC-SHA-256 of clientDataHash parameter using
        //    pinToken: HMAC-SHA-256(pinToken, clientDataHash).
        //     1. If the verification succeeds, set the "uv" bit to 1 in the response.
        //     2. If the verification fails, return CTAP2_ERR_PIN_AUTH_INVALID error.
        // 3. If pinAuth parameter is present and the pinProtocol is not supported,
        //    return CTAP2_ERR_PIN_AUTH_INVALID.
        // 4. If pinAuth parameter is not present and clientPin has been set on the authenticator,
        //    set the "uv" bit to 0 in the response.
        if input.pin_auth.is_some() {
            return Err(Ctap2Error::PinAuthInvalid.into());
        }

        // 5. If the options parameter is present, process all the options.
        //     1. If the option is known but not supported, terminate this procedure and
        //        return CTAP2_ERR_UNSUPPORTED_OPTION.
        //     2. If the option is known but not valid for this command, terminate this procedure
        //        and return CTAP2_ERR_INVALID_OPTION.
        //     3. Ignore any options that are not understood.
        // Note that because this specification defines normative behaviors for them, all
        // authenticators MUST understand the "rk", "up", and "uv" options.

        //    4. If the "rk" option is present then:
        //       1. Return CTAP2_ERR_UNSUPPORTED_OPTION.
        if input.options.rk {
            return Err(Ctap2Error::UnsupportedOption.into());
        }

        // 6. TODO, if the extensions parameter is present, process any extensions that this
        //    authenticator supports. Authenticator extension outputs generated by the authenticator
        //    extension processing are returned in the authenticator data.

        // 7. Collect user consent if required. This step MUST happen before the following steps due
        //    to privacy reasons (i.e., authenticator cannot disclose existence of a credential
        //    until the user interacted with the device):
        let flags = self
            .check_user(&input.options, maybe_credential.as_ref().ok())
            .await?;

        // 8. If no credentials were located in step 1, return CTAP2_ERR_NO_CREDENTIALS.
        let mut credential = maybe_credential?
            .try_into()
            .ok()
            .ok_or(Ctap2Error::NoCredentials)?;

        // 9. If more than one credential was located in step 1 and allowList is present and not
        //    empty, select any applicable credential and proceed to step 12. Otherwise, order the
        //    credentials by the time when they were created in reverse order. The first credential
        //    is the most recent credential that was created.
        // NB: This should be done within the `CredentialStore::find_any` implementation. Essentially
        // if multiple credentials are found, use the most recently created one.

        // 10. If authenticator does not have a display:
        //     1. Remember the authenticatorGetAssertion parameters.
        //     2. Create a credential counter(credentialCounter) and set it 1. This counter
        //        signifies how many credentials are sent to the platform by the authenticator.
        //     3. Start a timer. This is used during authenticatorGetNextAssertion command.
        //        This step is optional if transport is done over NFC.
        //     4. Update the response to include the first credential’s publicKeyCredentialUserEntity
        //        information and numberOfCredentials. User identifiable information (name,
        //        DisplayName, icon) inside publicKeyCredentialUserEntity MUST not be returned if
        //        user verification is not done by the authenticator.

        // 11. If authenticator has a display:
        //     1. Display all these credentials to the user, using their friendly name along with
        //        other stored account information.
        //     2. Also, display the rpId of the requester (specified in the request) and ask the
        //        user to select a credential.
        //     3. If the user declines to select a credential or takes too long (as determined by
        //        the authenticator), terminate this procedure and return the
        //        CTAP2_ERR_OPERATION_DENIED error.

        // [WebAuthn-9]. Increment the credential associated signature counter or the global signature
        //               counter value, depending on which approach is implemented by the authenticator,
        //               by some positive value. If the authenticator does not implement a signature
        //               counter, let the signature counter value remain constant at zero.
        if let Some(counter) = credential.counter {
            credential.counter = Some(counter + 1);
            self.store_mut()
                .update_credential(credential.clone())
                .await?;
        }

        let extensions =
            self.get_extensions(&credential, input.extensions, flags.contains(Flags::UV))?;
        // 12. Sign the clientDataHash along with authData with the selected credential.
        //     Let signature be the assertion signature of the concatenation `authenticatorData` ||
        //     `clien_data_hash` using the privateKey of selectedCredential. A simple, undelimited
        //      concatenation is safe to use here because the authenticator data describes its own
        //      length. The hash of the serialized client data (which potentially has a variable
        //      length) is always the last element.
        let auth_data = AuthenticatorData::new(&input.rp_id, credential.counter)
            .set_flags(flags)
            .set_assertion_extensions(extensions.signed)?;
        let mut signature_target = auth_data.to_vec();
        signature_target.extend(input.client_data_hash);

        let secret_key = private_key_from_cose_key(&credential.key)?;

        let mut private_key = SigningKey::from(secret_key);

        let signature: p256::ecdsa::Signature = private_key.sign(&signature_target);
        let signature_bytes = signature.to_der().to_bytes().to_vec().into();

        let user_handle = credential.user_handle.clone();

        Ok(Response {
            credential: Some(credential.into()),
            auth_data,
            signature: signature_bytes,
            user: user_handle.map(|id| PublicKeyCredentialUserEntity {
                id,
                // TODO: make a Authenticator version of this struct similar to make_credential::PublicKeyCredentialRpEntity
                // since these fields are optional at the authenticator boundry, but required at the client boundry.
                display_name: "".into(),
                name: "".into(),
            }),
            number_of_credentials: None,
            unsigned_extension_outputs: extensions.unsigned,
        })
    }
}

#[cfg(test)]
mod tests {
    use coset::{CborSerializable, CoseKey};
    use passkey_types::{
        ctap2::{
            get_assertion::{Options, Request},
            Aaguid,
        },
        Passkey,
    };

    use crate::{Authenticator, MockUserValidationMethod};

    fn create_passkey() -> Passkey {
        Passkey {
            key: private_key_for_testing(),
            credential_id: Default::default(),
            rp_id: "example.com".into(),
            user_handle: None,
            counter: None,
            extensions: Default::default(),
        }
    }

    fn good_request() -> Request {
        Request {
            rp_id: "example.com".into(),
            client_data_hash: vec![0; 32].into(),
            allow_list: None,
            extensions: None,
            pin_auth: None,
            pin_protocol: None,
            options: Options {
                up: true,
                uv: true,
                rk: false,
            },
        }
    }

    fn private_key_for_testing() -> CoseKey {
        // Hardcoded CoseKey for testing purposes
        let bytes = vec![
            166, 1, 2, 3, 38, 32, 1, 33, 88, 32, 200, 30, 161, 146, 196, 121, 165, 149, 92, 232,
            49, 48, 245, 253, 73, 234, 204, 3, 209, 153, 166, 77, 59, 232, 70, 16, 206, 77, 84,
            156, 28, 77, 34, 88, 32, 82, 141, 165, 28, 241, 82, 31, 33, 183, 206, 29, 91, 93, 111,
            216, 216, 26, 62, 211, 49, 191, 86, 238, 118, 241, 124, 131, 106, 214, 95, 170, 160,
            35, 88, 32, 147, 171, 4, 49, 68, 170, 47, 51, 74, 211, 94, 40, 212, 244, 95, 55, 154,
            92, 171, 241, 0, 55, 84, 151, 79, 244, 151, 198, 135, 45, 97, 238,
        ];

        CoseKey::from_slice(bytes.as_slice()).unwrap()
    }

    #[tokio::test]
    async fn get_assertion_increments_signature_counter_when_counter_is_some() {
        // Arrange
        let request = good_request();
        let store = Some(Passkey {
            counter: Some(9000),
            ..create_passkey()
        });
        let mut authenticator = Authenticator::new(
            Aaguid::new_empty(),
            store,
            MockUserValidationMethod::verified_user(1),
        );

        // Act
        let response = authenticator.get_assertion(request).await.unwrap();

        // Assert
        assert_eq!(response.auth_data.counter.unwrap(), 9001);
        assert_eq!(
            authenticator
                .store()
                .as_ref()
                .and_then(|c| c.counter)
                .unwrap(),
            9001
        );
    }
}
