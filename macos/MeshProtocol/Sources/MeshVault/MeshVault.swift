import CryptoKit
import Foundation
import LocalAuthentication
import MeshProtocol
import Security

/// Where a Silent Mesh identity lives at rest (Phase 4, D19).
///
/// **The Secure Enclave cannot hold the identity itself.** The SE does
/// P-256 only; a Nostr identity is secp256k1. So the enclave key is never
/// the identity — it is a *wrapping* key, generated inside the enclave and
/// non-extractable, used to encrypt the 32-byte secret at rest. Anyone who
/// copies the vault file to another machine has ciphertext no enclave will
/// ever unwrap, which is what "device-bound" means here.
///
/// Encryption uses the enclave key's **public** half, so saving needs no
/// biometric prompt; decryption uses the private half, so unlocking does.
/// That asymmetry matches how a vault is actually used — write often, read
/// deliberately.
public enum VaultProtection: Sendable, Equatable {
    /// Enclave-wrapped, unlockable without a prompt. Protects the secret at
    /// rest and binds it to this device; does not prove *who* is asking.
    case secureEnclave

    /// Enclave-wrapped, and every unlock requires Touch ID or the login
    /// password. This is the roadmap's "app relaunches locked".
    case secureEnclaveWithUserPresence

    /// No enclave: a passphrase-derived key, for hardware that has none.
    /// Chosen explicitly, never fallen back into — a vault that silently
    /// downgrades tells the member their key is protected by hardware when
    /// it is protected by a string.
    case softwarePassphrase
}

/// What went wrong, in terms a caller can act on.
public enum VaultError: Error, Equatable {
    /// This machine has no Secure Enclave.
    case enclaveUnavailable
    /// The OS refused the operation; carries the `OSStatus` and a reading.
    case keychain(OSStatus, String)
    /// The user cancelled the biometric prompt, or it failed.
    case authenticationFailed(String)
    /// The stored blob is absent, truncated, or not what this vault wrote.
    case corruptVault(String)
    /// The unwrapped bytes are not a usable identity.
    case notAnIdentity
}

/// An identity sealed at rest.
///
/// `MeshVault` owns only the wrapping and the storage. It hands back
/// `MeshKeys` on unlock and holds no plaintext of its own, so the window in
/// which the secret exists in memory is the caller's to keep short.
public struct MeshVault: Sendable {
    /// Keychain tag for the enclave key. One per vault file, derived from
    /// the vault's name so two workspaces on one Mac do not collide.
    private let keyTag: Data
    private let storeURL: URL
    private let protection: VaultProtection

    /// Version + protection prefix on the stored blob. A vault written by a
    /// future version must be refused rather than misread.
    private static let magic = "SMV1".data(using: .utf8)!

    public init(name: String = "default", directory: URL? = nil, protection: VaultProtection) {
        let base =
            directory
            ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("SilentMesh", isDirectory: true)
        storeURL = base.appendingPathComponent("\(name).meshvault")
        keyTag = Data("net.silentmesh.vault.\(name)".utf8)
        self.protection = protection
    }

    /// Does this machine have a Secure Enclave at all?
    ///
    /// Checked by *using* it — the availability of the hardware and the
    /// right to use it (code signing, entitlements) are different things,
    /// and only an attempt distinguishes them.
    public static func isSecureEnclaveAvailable() -> Bool {
        guard
            let access = SecAccessControlCreateWithFlags(
                nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, .privateKeyUsage, nil)
        else { return false }
        let attributes: [String: Any] = [
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits as String: 256,
            kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
            kSecPrivateKeyAttrs as String: [
                kSecAttrIsPermanent as String: false,
                kSecAttrAccessControl as String: access,
            ] as [String: Any],
        ]
        var error: Unmanaged<CFError>?
        let key = SecKeyCreateRandomKey(attributes as CFDictionary, &error)
        return key != nil
    }

    public var vaultExists: Bool { FileManager.default.fileExists(atPath: storeURL.path) }

    /// Seal `identity` into the vault, replacing anything already there.
    public func seal(identityHex: String) throws {
        guard let secret = MeshHex.decode(identityHex), secret.count == 32 else {
            throw VaultError.notAnIdentity
        }
        let payload = Self.magic + Data(secret)
        let sealed: Data
        switch protection {
        case .secureEnclave, .secureEnclaveWithUserPresence:
            let key = try enclaveKey(createIfMissing: true)
            guard let publicKey = SecKeyCopyPublicKey(key) else {
                throw VaultError.keychain(errSecInvalidKeyRef, "enclave key has no public half")
            }
            var error: Unmanaged<CFError>?
            guard
                let cipher = SecKeyCreateEncryptedData(
                    publicKey, .eciesEncryptionCofactorX963SHA256AESGCM,
                    payload as CFData, &error) as Data?
            else {
                throw VaultError.keychain(errSecParam, Self.describe(error))
            }
            sealed = cipher
        case .softwarePassphrase:
            throw VaultError.keychain(
                errSecUnimplemented,
                "software vaults are sealed with sealSoftware(identityHex:passphrase:)")
        }
        try write(sealed)
    }

    /// Seal without an enclave, deriving the key from a passphrase.
    public func sealSoftware(identityHex: String, passphrase: String) throws {
        guard let secret = MeshHex.decode(identityHex), secret.count == 32 else {
            throw VaultError.notAnIdentity
        }
        let salt = Data((0..<16).map { _ in UInt8.random(in: 0...255) })
        let key = Self.deriveKey(passphrase: passphrase, salt: salt)
        let box = try AES.GCM.seal(Self.magic + Data(secret), using: key)
        guard let combined = box.combined else {
            throw VaultError.corruptVault("AES-GCM produced no combined box")
        }
        try write(salt + combined)
    }

    /// Unlock the vault, prompting for Touch ID when the protection policy
    /// requires it.
    ///
    /// `reason` is shown in the system prompt: it is the only explanation
    /// the member gets for why their Mac is asking, so it should say what
    /// is about to happen, not "authenticate".
    public func unlock(reason: String = "Unlock your Silent Mesh identity") throws -> MeshKeys {
        let stored = try read()
        let payload: Data
        switch protection {
        case .secureEnclave, .secureEnclaveWithUserPresence:
            let key = try enclaveKey(createIfMissing: false, reason: reason)
            var error: Unmanaged<CFError>?
            guard
                let plain = SecKeyCreateDecryptedData(
                    key, .eciesEncryptionCofactorX963SHA256AESGCM,
                    stored as CFData, &error) as Data?
            else {
                let message = Self.describe(error)
                // A cancelled prompt is a decision, not a failure of the
                // vault, and the caller should be able to tell them apart.
                if message.localizedCaseInsensitiveContains("cancel")
                    || message.localizedCaseInsensitiveContains("authentication")
                {
                    throw VaultError.authenticationFailed(message)
                }
                throw VaultError.keychain(errSecDecode, message)
            }
            payload = plain
        case .softwarePassphrase:
            throw VaultError.keychain(
                errSecUnimplemented, "software vaults open with unlockSoftware(passphrase:)")
        }
        return try Self.identity(from: payload)
    }

    /// Unlock a software vault.
    public func unlockSoftware(passphrase: String) throws -> MeshKeys {
        let stored = try read()
        guard stored.count > 16 else { throw VaultError.corruptVault("too short for a salt") }
        let salt = stored.prefix(16)
        let key = Self.deriveKey(passphrase: passphrase, salt: Data(salt))
        guard let box = try? AES.GCM.SealedBox(combined: Data(stored.dropFirst(16))),
            let payload = try? AES.GCM.open(box, using: key)
        else {
            // Wrong passphrase and a tampered blob are indistinguishable to
            // AES-GCM, and saying which would be a hint worth having.
            throw VaultError.authenticationFailed("wrong passphrase, or the vault was altered")
        }
        return try Self.identity(from: payload)
    }

    /// Destroy the vault and its enclave key. Irreversible: the wrapped
    /// secret becomes undecryptable the moment the enclave key is gone,
    /// which is the point.
    public func destroy() throws {
        try? FileManager.default.removeItem(at: storeURL)
        let query: [String: Any] = [
            kSecClass as String: kSecClassKey,
            kSecAttrApplicationTag as String: keyTag,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw VaultError.keychain(status, "could not delete the enclave key")
        }
    }

    // MARK: - Internals

    private func enclaveKey(createIfMissing: Bool, reason: String? = nil) throws -> SecKey {
        var query: [String: Any] = [
            kSecClass as String: kSecClassKey,
            kSecAttrApplicationTag as String: keyTag,
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecReturnRef as String: true,
        ]
        if let reason {
            let context = LAContext()
            context.localizedReason = reason
            query[kSecUseAuthenticationContext as String] = context
        }
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecSuccess, let key = item {
            // swift-format-ignore: NeverForceUnwrap
            return (key as! SecKey)
        }
        guard status == errSecItemNotFound, createIfMissing else {
            throw VaultError.keychain(status, Self.reading(status))
        }
        return try createEnclaveKey()
    }

    private func createEnclaveKey() throws -> SecKey {
        var flags: SecAccessControlCreateFlags = [.privateKeyUsage]
        if protection == .secureEnclaveWithUserPresence {
            // `.userPresence` accepts Touch ID or the login password, so a
            // member whose finger is not recognised is inconvenienced, not
            // locked out of their own workspace.
            flags.insert(.userPresence)
        }
        var accessError: Unmanaged<CFError>?
        guard
            let access = SecAccessControlCreateWithFlags(
                nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, flags, &accessError)
        else {
            throw VaultError.keychain(errSecParam, Self.describe(accessError))
        }
        let attributes: [String: Any] = [
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits as String: 256,
            kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
            kSecPrivateKeyAttrs as String: [
                kSecAttrIsPermanent as String: true,
                kSecAttrApplicationTag as String: keyTag,
                kSecAttrAccessControl as String: access,
            ] as [String: Any],
        ]
        var error: Unmanaged<CFError>?
        guard let key = SecKeyCreateRandomKey(attributes as CFDictionary, &error) else {
            let message = Self.describe(error)
            if message.contains("-34018") || message.localizedCaseInsensitiveContains("entitlement")
            {
                throw VaultError.keychain(
                    -34018,
                    "the enclave refused an unsigned or unentitled app: \(message)")
            }
            throw VaultError.enclaveUnavailable
        }
        return key
    }

    private func write(_ data: Data) throws {
        let directory = storeURL.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try data.write(to: storeURL, options: [.atomic, .completeFileProtection])
    }

    private func read() throws -> Data {
        guard let data = FileManager.default.contents(atPath: storeURL.path) else {
            throw VaultError.corruptVault("no vault at \(storeURL.path)")
        }
        return data
    }

    private static func identity(from payload: Data) throws -> MeshKeys {
        guard payload.count == magic.count + 32, payload.prefix(magic.count) == magic else {
            throw VaultError.corruptVault("not a Silent Mesh vault payload")
        }
        let secret = payload.suffix(32)
        guard let keys = try? MeshKeys(privateKeyHex: MeshHex.encode(secret)) else {
            throw VaultError.notAnIdentity
        }
        return keys
    }

    private static func deriveKey(passphrase: String, salt: Data) -> SymmetricKey {
        // HKDF over the passphrase. Adequate for a deliberately-secondary
        // path; a shipped software vault wants a memory-hard KDF, and that
        // choice belongs with the decision to ship one.
        let material = SymmetricKey(data: Data(passphrase.utf8))
        return HKDF<SHA256>.deriveKey(inputKeyMaterial: material, salt: salt, outputByteCount: 32)
    }

    private static func describe(_ error: Unmanaged<CFError>?) -> String {
        guard let error else { return "unknown error" }
        return (error.takeRetainedValue() as Error).localizedDescription
    }

    private static func reading(_ status: OSStatus) -> String {
        SecCopyErrorMessageString(status, nil) as String? ?? "OSStatus \(status)"
    }
}
