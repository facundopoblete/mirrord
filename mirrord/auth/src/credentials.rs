use std::fmt::Debug;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
pub use x509_certificate;
use x509_certificate::{asn1time::Time, rfc5280};
#[cfg(feature = "client")]
use x509_certificate::{
    rfc2986, InMemorySigningKeyPair, X509CertificateBuilder, X509CertificateError,
};

use crate::{certificate::Certificate, key_pair::KeyPair};

/// Client credentials container for authentication with the operator.
/// Contains a local [`KeyPair`] and an optional [`Certificate`].
#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    /// Certificate generated by the operator based on the sent [`rfc2986::CertificationRequest`].
    certificate: Certificate,
    /// Local key pair for creating [`rfc2986::CertificationRequest`]s for operator.
    /// This key pair does not change when generating a new request with
    /// [`Credentials::certificate_request`].
    key_pair: KeyPair,
}

impl Credentials {
    /// Returns the key pair used to sign certification requests.
    pub fn key_pair(&self) -> &KeyPair {
        &self.key_pair
    }

    /// Checks if [`Certificate`] in this struct is valid in terms of expiration.
    pub fn is_valid(&self) -> bool {
        self.certificate
            .as_ref()
            .tbs_certificate
            .validity
            .is_date_valid(Utc::now())
    }

    /// Creates [`rfc2986::CertificationRequest`] for [`Certificate`] generation in the operator.
    #[cfg(feature = "client")]
    fn certificate_request(
        common_name: &str,
        key_pair: &InMemorySigningKeyPair,
    ) -> Result<rfc2986::CertificationRequest, X509CertificateError> {
        let mut builder = X509CertificateBuilder::default();

        let _ = builder
            .subject()
            .append_common_name_utf8_string(common_name);

        builder.create_certificate_signing_request(key_pair)
    }
}

impl AsRef<Certificate> for Credentials {
    fn as_ref(&self) -> &Certificate {
        &self.certificate
    }
}

/// Extends a date type ([`DateTime<Utc>`]) to help us when checking for a license's
/// certificate validity.
///
/// Also implemented for [`NaiveDate`], because that's what we get from operator status.
pub trait LicenseValidity {
    /// How many days we consider a license is close to expiring.
    ///
    /// You can access this constant as
    /// `<DateTime<Utc> as LicenseValidity>::CLOSE_TO_EXPIRATION_DAYS`.
    const CLOSE_TO_EXPIRATION_DAYS: u64 = 2;

    /// This date's validity is good.
    fn is_good(&self) -> bool;

    /// How many days until expiration from this date counting from _now_, which means that an
    /// expiration date of `today + 3` means we have 2 days left until expiry.
    fn days_until_expiration(&self) -> Option<u64>;
}

impl LicenseValidity for DateTime<Utc> {
    fn is_good(&self) -> bool {
        Utc::now() < *self
    }

    fn days_until_expiration(&self) -> Option<u64> {
        self.signed_duration_since(Utc::now())
            .num_days()
            .try_into()
            .ok()
    }
}

impl LicenseValidity for NaiveDate {
    fn is_good(&self) -> bool {
        Utc::now().naive_utc().date() <= *self
    }

    fn days_until_expiration(&self) -> Option<u64> {
        self.signed_duration_since(Utc::now().naive_utc().date())
            .num_days()
            .try_into()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Days, Utc};

    use crate::credentials::LicenseValidity;

    #[test]
    fn license_validity_valid() {
        let today: DateTime<Utc> = Utc::now();
        let expiration_date = today.checked_add_days(Days::new(7)).unwrap();

        assert!(expiration_date.is_good());
    }

    #[test]
    fn license_validity_expired() {
        let today: DateTime<Utc> = Utc::now();
        let expiration_date = today.checked_sub_days(Days::new(7)).unwrap();

        assert!(!expiration_date.is_good());
    }

    #[test]
    fn license_validity_close_to_expiring() {
        let today: DateTime<Utc> = Utc::now();
        let expiration_date = today.checked_add_days(Days::new(3)).unwrap();

        assert_eq!(expiration_date.days_until_expiration(), Some(2));
    }

    #[test]
    fn license_validity_valid_naive() {
        let today = Utc::now().naive_utc().date();
        let expiration_date = today.checked_add_days(Days::new(7)).unwrap();

        assert!(expiration_date.is_good())
    }

    #[test]
    fn license_validity_expired_naive() {
        let today = Utc::now().naive_utc().date();
        let expiration_date = today.checked_sub_days(Days::new(7)).unwrap();

        assert!(!expiration_date.is_good());
    }

    #[test]
    fn license_validity_close_to_expiring_naive() {
        let today = Utc::now().naive_utc().date();
        let expiration_date = today.checked_add_days(Days::new(3)).unwrap();

        assert_eq!(expiration_date.days_until_expiration(), Some(3));
    }

    #[test]
    fn license_validity_same_day_naive() {
        let today = Utc::now().naive_utc().date();
        let expiration_date = today;

        assert!(expiration_date.is_good());
        assert_eq!(expiration_date.days_until_expiration(), Some(0));
    }
}

/// Ext trait for validation of dates of `rfc5280::Validity`
pub trait DateValidityExt {
    /// Check other is in between not_before and not_after
    fn is_date_valid(&self, other: DateTime<Utc>) -> bool;
}

impl DateValidityExt for rfc5280::Validity {
    fn is_date_valid(&self, other: DateTime<Utc>) -> bool {
        let not_before: DateTime<Utc> = match self.not_before.clone() {
            Time::UtcTime(time) => *time,
            Time::GeneralTime(time) => DateTime::<Utc>::from(time),
        };

        let not_after: DateTime<Utc> = match self.not_after.clone() {
            Time::UtcTime(time) => *time,
            Time::GeneralTime(time) => DateTime::<Utc>::from(time),
        };

        not_before < other && other < not_after
    }
}

/// Extenstion of Credentials for functions that accesses Operator
#[cfg(feature = "client")]
pub mod client {
    use kube::{api::PostParams, Api, Client, Resource};

    use super::*;
    use crate::error::CredentialStoreError;

    impl Credentials {
        /// Create a [`rfc2986::CertificationRequest`] and send it to the operator.
        /// If the `key_pair` is not given, the request is signed with a randomly generated one.
        pub async fn init<R>(
            client: Client,
            common_name: &str,
            key_pair: Option<KeyPair>,
        ) -> Result<Self, CredentialStoreError>
        where
            R: Resource + Clone + Debug,
            R: for<'de> Deserialize<'de>,
            R::DynamicType: Default,
        {
            let key_pair = match key_pair {
                Some(key_pair) => key_pair,
                None => KeyPair::new_random()?,
            };

            let certificate_request = Self::certificate_request(common_name, &key_pair)?
                .encode_pem()
                .map_err(X509CertificateError::from)?;

            let api: Api<R> = Api::all(client);

            let certificate: Certificate = api
                .create_subresource(
                    "certificate",
                    "operator",
                    &PostParams::default(),
                    certificate_request.into(),
                )
                .await?;

            Ok(Credentials {
                certificate,
                key_pair,
            })
        }

        /// Create [`rfc2986::CertificationRequest`] and send it to the operator.
        /// Returned certificate replaces the [`Certificate`] stored in this struct.
        pub async fn refresh<R>(
            &mut self,
            client: Client,
            common_name: &str,
        ) -> Result<(), CredentialStoreError>
        where
            R: Resource + Clone + Debug,
            R: for<'de> Deserialize<'de>,
            R::DynamicType: Default,
        {
            let certificate_request = Self::certificate_request(common_name, &self.key_pair)?
                .encode_pem()
                .map_err(X509CertificateError::from)?;

            let api: Api<R> = Api::all(client);

            let certificate: Certificate = api
                .create_subresource(
                    "certificate",
                    "operator",
                    &PostParams::default(),
                    certificate_request.into(),
                )
                .await?;

            self.certificate = certificate;

            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use bcder::{
        decode::{BytesSource, Constructed},
        Mode,
    };
    use x509_certificate::rfc2986::CertificationRequest;

    /// Verifies that [`CertificationRequest`] properly decodes from value produced by old code.
    #[test]
    fn decode_old_certificate_request() {
        const REQUEST: &str = "PEM: -----BEGIN CERTIFICATE REQUEST-----
MIGXMEkCAQAwFDESMBAGA1UEAwwJc29tZV9uYW1lMCwwBwYDK2VuBQADIQDhLn5T
fFTb4xOq+a1HyC3T7ScFiQGBy+oUcwFiCVCUI6AAMAcGAytlcAUAA0EAPBRvsUHo
+J/INwq6tn5kgcE9vMo48kRkyhWSp3XmfuUvxW/b7LufrlTcjw+4RG8pdugMXhcz
5+u20nm4VY+sCg==
-----END CERTIFICATE REQUEST-----
";
        const PUBLIC_KEY: &[u8] =  b"\xe1.~S|T\xdb\xe3\x13\xaa\xf9\xadG\xc8-\xd3\xed'\x05\x89\x01\x81\xcb\xea\x14s\x01b\tP\x94#";

        let certification_request_pem = pem::parse(REQUEST).unwrap();
        let certification_request_source =
            BytesSource::new(certification_request_pem.into_contents().into());
        let certification_request = Constructed::decode(
            certification_request_source,
            Mode::Der,
            CertificationRequest::take_from,
        )
        .unwrap();

        assert_eq!(
            certification_request
                .certificate_request_info
                .subject
                .iter_common_name()
                .next()
                .unwrap()
                .to_string()
                .unwrap(),
            "some_name"
        );
        assert_eq!(
            certification_request
                .certificate_request_info
                .subject_public_key_info
                .subject_public_key
                .octet_bytes(),
            PUBLIC_KEY
        );
    }
}
