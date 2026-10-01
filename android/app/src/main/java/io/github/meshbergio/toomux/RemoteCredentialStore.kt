package io.github.meshbergio.toomux

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

data class RemoteCredential(val endpoint: String, val token: String, val deviceId: String)

/**
 * The bearer token is encrypted at rest with a non-exportable Android
 * Keystore AES key. The endpoint and opaque device id are not secrets.
 */
class RemoteCredentialStore(private val context: Context) {
    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun load(): RemoteCredential? {
        val endpoint = prefs.getString(KEY_ENDPOINT, null) ?: return null
        val deviceId = prefs.getString(KEY_DEVICE_ID, null) ?: return null
        val blob = prefs.getString(KEY_TOKEN, null) ?: return null
        return try {
            RemoteCredential(endpoint, decrypt(blob), deviceId)
        } catch (_: Exception) {
            clear()
            null
        }
    }

    fun save(credential: RemoteCredential) {
        prefs.edit()
            .putString(KEY_ENDPOINT, credential.endpoint)
            .putString(KEY_DEVICE_ID, credential.deviceId)
            .putString(KEY_TOKEN, encrypt(credential.token))
            .apply()
    }

    fun rememberedEndpoint(): String =
        prefs.getString(KEY_ENDPOINT, ToomuxApi.DEFAULT_ENDPOINT) ?: ToomuxApi.DEFAULT_ENDPOINT

    fun clear() {
        prefs.edit().clear().apply()
        runCatching {
            val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
            if (keyStore.containsAlias(KEY_ALIAS)) keyStore.deleteEntry(KEY_ALIAS)
        }
    }

    private fun encrypt(value: String): String {
        val cipher = Cipher.getInstance(CIPHER)
        cipher.init(Cipher.ENCRYPT_MODE, getOrCreateKey())
        val ciphertext = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        val iv = Base64.encodeToString(cipher.iv, Base64.NO_WRAP)
        val data = Base64.encodeToString(ciphertext, Base64.NO_WRAP)
        return "$iv.$data"
    }

    private fun decrypt(blob: String): String {
        val parts = blob.split('.', limit = 2)
        require(parts.size == 2)
        val iv = Base64.decode(parts[0], Base64.NO_WRAP)
        val ciphertext = Base64.decode(parts[1], Base64.NO_WRAP)
        val cipher = Cipher.getInstance(CIPHER)
        cipher.init(Cipher.DECRYPT_MODE, getOrCreateKey(), GCMParameterSpec(128, iv))
        return String(cipher.doFinal(ciphertext), Charsets.UTF_8)
    }

    private fun getOrCreateKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setRandomizedEncryptionRequired(true)
                .build(),
        )
        return generator.generateKey()
    }

    companion object {
        private const val PREFS = "toomux_remote"
        private const val KEY_ENDPOINT = "endpoint"
        private const val KEY_DEVICE_ID = "device_id"
        private const val KEY_TOKEN = "token_ciphertext"
        private const val KEY_ALIAS = "toomux_remote_token_v1"
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val CIPHER = "AES/GCM/NoPadding"
    }
}
