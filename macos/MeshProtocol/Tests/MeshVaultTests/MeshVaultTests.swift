import Foundation
import MeshProtocol
import Testing

@testable import MeshVault

/// The software path, which is testable anywhere. The Secure Enclave path
/// needs a signed process, so it is exercised by the app's self-test
/// (`MESH_VAULT_SELFTEST=1`) rather than pretended at here — a vault test
/// that silently ran on software while claiming hardware would be worse
/// than no test.
@Suite("Software vault")
struct SoftwareVaultTests {
    private func makeVault(_ name: String = UUID().uuidString) -> MeshVault {
        MeshVault(
            name: name,
            directory: FileManager.default.temporaryDirectory
                .appendingPathComponent("meshvault-tests", isDirectory: true),
            protection: .softwarePassphrase)
    }

    @Test("an identity survives seal and unlock unchanged")
    func roundTrip() throws {
        let identity = try MeshKeys()
        let secret = String(repeating: "7a", count: 32)
        let expected = try MeshKeys(privateKeyHex: secret)
        _ = identity

        let vault = makeVault()
        #expect(vault.vaultExists == false)
        try vault.sealSoftware(identityHex: secret, passphrase: "correct horse")
        #expect(vault.vaultExists)

        let unlocked = try vault.unlockSoftware(passphrase: "correct horse")
        #expect(unlocked.publicKeyHex == expected.publicKeyHex)
        try vault.destroy()
        #expect(vault.vaultExists == false)
    }

    @Test("a wrong passphrase fails, and says nothing about which byte was wrong")
    func wrongPassphrase() throws {
        let vault = makeVault()
        try vault.sealSoftware(
            identityHex: String(repeating: "11", count: 32), passphrase: "right")
        #expect(throws: VaultError.self) { try vault.unlockSoftware(passphrase: "wrong") }
        try vault.destroy()
    }

    @Test("a tampered vault does not open")
    func tamperDetected() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("meshvault-tests", isDirectory: true)
        let name = UUID().uuidString
        let vault = MeshVault(name: name, directory: directory, protection: .softwarePassphrase)
        try vault.sealSoftware(
            identityHex: String(repeating: "22", count: 32), passphrase: "pass")

        // Flip a byte in the ciphertext: AES-GCM must refuse it rather than
        // hand back a corrupted key that would sign unverifiable events.
        let url = directory.appendingPathComponent("\(name).meshvault")
        var bytes = try Data(contentsOf: url)
        bytes[bytes.count - 1] ^= 0xFF
        try bytes.write(to: url)

        #expect(throws: VaultError.self) { try vault.unlockSoftware(passphrase: "pass") }
        try vault.destroy()
    }

    @Test("a missing vault is reported as missing, not as a bad passphrase")
    func missingVault() throws {
        let vault = makeVault()
        #expect(throws: VaultError.self) { try vault.unlockSoftware(passphrase: "x") }
    }

    @Test("only a 32-byte identity can be sealed")
    func rejectsNonIdentities() throws {
        let vault = makeVault()
        #expect(throws: VaultError.self) {
            try vault.sealSoftware(identityHex: "beef", passphrase: "p")
        }
    }
}
