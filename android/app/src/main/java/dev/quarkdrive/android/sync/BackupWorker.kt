package dev.quarkdrive.android.sync

import android.content.ContentUris
import android.content.Context
import android.provider.MediaStore
import android.util.Log
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import dev.quarkdrive.android.QuarkdriveNative
import dev.quarkdrive.android.api.ApiClient
import dev.quarkdrive.android.data.Settings
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext
import java.util.concurrent.TimeUnit

/**
 * Automatic camera backup — the Immich half of Quarkdrive.
 *
 * Runs periodically, finds photos added since the last run, and uploads the
 * ones whose content is not already in the vault.
 *
 * Two details make this cheap enough to run on a phone:
 *
 *  - it only looks at photos newer than the previous run, so a steady state
 *    costs one MediaStore query;
 *  - it hashes each photo with the Rust core and skips anything whose content
 *    address is already backed up. Re-enabling backup, or moving a photo
 *    between albums, therefore does not re-upload it.
 */
class BackupWorker(appContext: Context, params: WorkerParameters) :
    CoroutineWorker(appContext, params) {

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val settings = Settings(applicationContext)
        val config = settings.config.first() ?: return@withContext Result.success()
        if (!config.autoBackup) return@withContext Result.success()

        val api = ApiClient(config.server, config.token)
        val known = settings.backedUpHashes.first().toMutableSet()
        val since = settings.lastBackupTime()
        val uploaded = mutableListOf<String>()
        var skipped = 0
        var latest = since

        try {
            val projection = arrayOf(
                MediaStore.Images.Media._ID,
                MediaStore.Images.Media.DISPLAY_NAME,
                MediaStore.Images.Media.DATE_ADDED,
            )
            val selection = "${MediaStore.Images.Media.DATE_ADDED} > ?"
            val selectionArgs = arrayOf(since.toString())
            val sortOrder = "${MediaStore.Images.Media.DATE_ADDED} ASC"

            applicationContext.contentResolver.query(
                MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
                projection,
                selection,
                selectionArgs,
                sortOrder,
            )?.use { cursor ->
                val idCol = cursor.getColumnIndexOrThrow(MediaStore.Images.Media._ID)
                val nameCol = cursor.getColumnIndexOrThrow(MediaStore.Images.Media.DISPLAY_NAME)
                val dateCol = cursor.getColumnIndexOrThrow(MediaStore.Images.Media.DATE_ADDED)

                while (cursor.moveToNext() && !isStopped) {
                    val id = cursor.getLong(idCol)
                    val displayName = cursor.getString(nameCol) ?: "$id.jpg"
                    val added = cursor.getLong(dateCol)
                    if (added > latest) latest = added

                    val uri = ContentUris.withAppendedId(
                        MediaStore.Images.Media.EXTERNAL_CONTENT_URI, id
                    )
                    val bytes = applicationContext.contentResolver
                        .openInputStream(uri)?.use { it.readBytes() } ?: continue

                    val hash = QuarkdriveNative.contentHash(bytes)
                    if (known.contains(hash)) {
                        skipped++
                        continue
                    }

                    val remote = "Camera/${sanitise(displayName)}"
                    runCatching { api.upload(config.vault, remote, bytes) }
                        .onSuccess {
                            known.add(hash)
                            uploaded.add(hash)
                        }
                        .onFailure { Log.w(TAG, "upload failed for $remote", it) }
                }
            }

            if (uploaded.isNotEmpty()) settings.markBackedUp(uploaded)
            settings.recordBackupTime(latest)

            Log.i(TAG, "backup finished: ${uploaded.size} uploaded, $skipped already present")
            Result.success(
                workDataOf(KEY_UPLOADED to uploaded.size, KEY_SKIPPED to skipped)
            )
        } catch (e: Exception) {
            Log.w(TAG, "backup run failed", e)
            if (runAttemptCount < MAX_ATTEMPTS) Result.retry() else Result.failure()
        }
    }

    private fun sanitise(name: String): String =
        name.replace(Regex("[/\\\\:*?\"<>|]"), "_").ifBlank { "photo.jpg" }

    companion object {
        private const val TAG = "BackupWorker"
        private const val MAX_ATTEMPTS = 3
        const val KEY_UPLOADED = "uploaded"
        const val KEY_SKIPPED = "skipped"
        const val WORK_NAME = "quarkdrive-camera-backup"
    }
}

/** Enqueues and cancels the periodic backup job. */
object BackupScheduler {

    /** 15 minutes is the shortest interval WorkManager allows. */
    private val INTERVAL_MINUTES = 15L

    fun enable(context: Context) {
        val constraints = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED)
            .build()
        val request = PeriodicWorkRequestBuilder<BackupWorker>(
            INTERVAL_MINUTES, TimeUnit.MINUTES
        )
            .setConstraints(constraints)
            .build()
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(
            BackupWorker.WORK_NAME,
            ExistingPeriodicWorkPolicy.KEEP,
            request,
        )
    }

    fun disable(context: Context) {
        WorkManager.getInstance(context).cancelUniqueWork(BackupWorker.WORK_NAME)
    }

    /** Upload anything new right now, rather than waiting for the next period. */
    fun runOnce(context: Context) {
        val request = androidx.work.OneTimeWorkRequestBuilder<BackupWorker>().build()
        WorkManager.getInstance(context).enqueue(request)
    }
}
