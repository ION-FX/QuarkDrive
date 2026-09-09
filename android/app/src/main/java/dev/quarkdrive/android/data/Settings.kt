package dev.quarkdrive.android.data

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.emptyPreferences
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.core.stringSetPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import java.io.IOException

private val Context.dataStore by preferencesDataStore(name = "quarkdrive")

/** Everything the app needs to talk to a vault. */
data class Config(
    val server: String,
    val token: String,
    val vault: String,
    val autoBackup: Boolean,
)

class Settings(private val context: Context) {

    private object Keys {
        val SERVER = stringPreferencesKey("server")
        val TOKEN = stringPreferencesKey("token")
        val VAULT = stringPreferencesKey("vault")
        val AUTO_BACKUP = booleanPreferencesKey("auto_backup")
        val BACKED_UP = stringSetPreferencesKey("backed_up_hashes")
        val LAST_BACKUP = longPreferencesKey("last_backup_epoch_seconds")
    }

    /** Null until the user has signed in and picked a vault. */
    val config: Flow<Config?> = context.dataStore.data
        .catch { if (it is IOException) emit(emptyPreferences()) else throw it }
        .map { prefs ->
            val server = prefs[Keys.SERVER] ?: return@map null
            val token = prefs[Keys.TOKEN] ?: return@map null
            val vault = prefs[Keys.VAULT] ?: return@map null
            Config(server, token, vault, prefs[Keys.AUTO_BACKUP] ?: true)
        }

    suspend fun signIn(server: String, token: String, vault: String) {
        context.dataStore.edit { prefs ->
            prefs[Keys.SERVER] = server.trimEnd('/')
            prefs[Keys.TOKEN] = token
            prefs[Keys.VAULT] = vault
        }
    }

    suspend fun setAutoBackup(enabled: Boolean) {
        context.dataStore.edit { it[Keys.AUTO_BACKUP] = enabled }
    }

    suspend fun clear() {
        context.dataStore.edit { it.clear() }
    }

    // ------------------------------------------------------- backup ledger

    /**
     * Content addresses already uploaded.
     *
     * Keyed by content rather than by MediaStore id, so renaming a photo, or
     * the same photo existing in two albums, does not cause a re-upload.
     */
    val backedUpHashes: Flow<Set<String>> = context.dataStore.data
        .catch { if (it is IOException) emit(emptyPreferences()) else throw it }
        .map { it[Keys.BACKED_UP] ?: emptySet() }

    suspend fun markBackedUp(hashes: Collection<String>) {
        if (hashes.isEmpty()) return
        context.dataStore.edit { prefs ->
            prefs[Keys.BACKED_UP] = (prefs[Keys.BACKED_UP] ?: emptySet()) + hashes
        }
    }

    suspend fun lastBackupTime(): Long =
        context.dataStore.data.first()[Keys.LAST_BACKUP] ?: 0L

    suspend fun recordBackupTime(epochSeconds: Long) {
        context.dataStore.edit { it[Keys.LAST_BACKUP] = epochSeconds }
    }
}
