import CopadCore
import Foundation

/// Glue between the credential index (metadata from `copad-core`, `CredentialRef` has no field
/// for secret material, and an agent may read it) and the keychain. The only code that holds
/// plaintext is `BrowserTab.fillCredential`, which hands it straight to the page.
@MainActor
enum BrowserSecrets {
    static func list(origin: String?) -> [CredentialRef] {
        BrowserFFI.listCredentials(origin: origin)
    }

    static func find(id: String) -> CredentialRef? {
        BrowserFFI.listCredentials(origin: nil).first { $0.id == id }
    }

    /// Read the password out of a protected page and store it.
    ///
    /// The value is read natively from a document automation was never in, and is not an RPC
    /// parameter, so an agent never handles it. Refused on an unprotected tab, where the value was
    /// already readable via `query` and `execute_js`.
    static func saveFromPage(
        tab: BrowserTab,
        username: String,
        completion: @escaping (Result<CredentialRef, RPCError>) -> Void,
    ) {
        guard tab.tabMode == .protected else {
            completion(.failure(RPCError(
                code: "requires_protected",
                message: "a password may only be captured from a protected tab",
            )))
            return
        }
        let origin = BrowserFFI.canonicalize(tab.currentURL, policy: "origin").url
        guard !origin.isEmpty else {
            completion(.failure(RPCError(
                code: "origin_mismatch",
                message: "the tab is not on an http(s) origin",
            )))
            return
        }
        let generationAtStart = tab.documentGeneration
        // The page reports its own origin with the value. Trusting an origin captured before the
        // async evaluation would let a mid-read navigation file another site's password under it.
        // Whether the credential already existed decides what a failed index write may roll back.
        let existedBefore = find(id: "\(origin)/\(username)") != nil
        let js = """
        (() => {
            const el = document.querySelector('input[type="password"]');
            if (!el || !el.value) return JSON.stringify(null);
            return JSON.stringify({ value: el.value, origin: location.origin });
        })()
        """
        let box = SendableBox<(Result<CredentialRef, RPCError>) -> Void>(completion)
        tab.executeJS(js) { result, _ in
            Task { @MainActor in
                guard let raw = result as? String,
                      let data = raw.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let secret = obj["value"] as? String, !secret.isEmpty,
                      let pageOrigin = obj["origin"] as? String
                else {
                    box.value(.failure(RPCError(
                        code: "no_target",
                        message: "no non-empty password field on this page",
                    )))
                    return
                }
                guard tab.documentGeneration == generationAtStart,
                      tab.tabMode == .protected,
                      pageOrigin.caseInsensitiveCompare(origin) == .orderedSame
                else {
                    box.value(.failure(RPCError(
                        code: "document_changed",
                        message: "the tab changed while the password was being read",
                    )))
                    return
                }
                // The id carries the whole origin, scheme included: otherwise http and https
                // origins would share a keychain account while `validate_fill` treats them as
                // different.
                let ref = CredentialRef(
                    id: "\(origin)/\(username)",
                    origin: origin,
                    username: username,
                    slot: "password",
                    createdAt: UInt64(Date().timeIntervalSince1970),
                )
                BrowserKeychain.save(
                    id: ref.id, secret: secret, origin: origin, slot: "password",
                ) { saved in
                    switch saved {
                    case let .failure(err):
                        box.value(.failure(BrowserTab.keychainError(err)))
                    case .success:
                        // Index last: an index entry with no stored secret offers a credential
                        // that cannot be filled.
                        guard BrowserFFI.upsertCredential(ref) else {
                            // Roll back only a credential this call created; deleting an existing
                            // one would destroy the password its index row still points at.
                            if !existedBefore {
                                BrowserKeychain.delete(id: ref.id)
                            }
                            box.value(.failure(RPCError(
                                code: "index_write_failed",
                                message: existedBefore
                                    ? "could not update the credential record; the stored secret was left as it is"
                                    : "could not record the credential; the secret was rolled back",
                            )))
                            return
                        }
                        box.value(.success(ref))
                    }
                }
            }
        }
    }

    /// Forget a credential. Keychain first: an index entry removed while its secret survives is an
    /// orphan nothing can clean up.
    static func delete(
        id: String,
        completion: @escaping (Result<Bool, RPCError>) -> Void,
    ) {
        let box = SendableBox<(Result<Bool, RPCError>) -> Void>(completion)
        BrowserKeychain.delete(id: id) { result in
            if case let .failure(err) = result, case .unavailable = err {
                box.value(.failure(BrowserTab.keychainError(err)))
                return
            }
            box.value(.success(BrowserFFI.removeCredential(id: id)))
        }
    }
}
