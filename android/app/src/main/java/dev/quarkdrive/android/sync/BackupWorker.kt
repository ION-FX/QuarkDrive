package dev.quarkdrive.android.sync

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.ContentUris
import android.content.Context
import android.content.pm.ServiceInfo
import android.os.Build
import android.provider.MediaStore
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ForegroundInfo
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
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
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.TimeUnit

/**
 * Automatic camera backup — the Immich half of Quarkdrive.
 *
 * Runs periodically, finds photos added since the last successful run, and
 * uploads the ones whose content is not already in the vault.
 *
 * Three details make this cheap enough, and safe enough, to run on a phone:
 *
 *  - it only looks at photos newer than the previous run, so a steady state
 *    costs one MediaStore query;
 *  - it hashes each photo with the Rust core and skips anything whose content
 *    address is already backed up. Re-enabling backup, or moving a photo
 *    between albums, therefore does not re-upload it;
 *  - the "last run" watermark only advances past photos that actually
 *    succeeded, so a failure is retried on the next run rather than being
 *    skipped for ever.
 */
class BackupWorker(appContext: Context, params: WorkerParameters) :
    CoroutineWorker(appContext, params) {

    override suspend fun getForegroundInfo(): ForegroundInfo = foregroundInfo(0, 0)

    private fun foregroundInfo(done: Int, total: Int): ForegroundInfo {
        val manager = applicationContext
            .getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    "Camera backup",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { description = "Progress while photos upload to your vault." }
            )
        }
        val text = if (total > 0) "Uploading photo $done of $total" else "Looking for new photos"
        val notification = NotificationCompat.Builder(applicationContext, CHANNEL_ID)
            .setContentTitle("Quarkdrive backup")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .apply { if (total > 0) setProgress(total, done, false) }
            .build()

        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ForegroundInfo(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            ForegroundInfo(NOTIFICATION_ID, notification)
        }
    }

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val settings = Settings(applicationContext)
        val config = settings.config.first() ?: return@withContext Result.success()
        if (!config.autoBackup) return@withContext Result.success()

        // A long first backup would otherwise be killed at the ten minute
        // execution limit. Not fatal if the promotion is refused.
        runCatching { setForeground(foregroundInfo(0, 0)) }

        val api = ApiClient(config.server, config.token)
        val known = settings.backedUpHashes.first().toMutableSet()
        val since = settings.lastBackupTime()

        // Hashes uploaded but not yet written to the ledger.
        val unsaved = mutableListOf<String>()
        var uploadedCount = 0
        var skipped = 0
        var failed = 0
        var oversize = 0

        // Only advances past photos that succeeded. Once one fails, it stops,
        // so the next run reconsiders everything from that point. Later
        // photos that did succeed are cheap to reconsider: their hash is in
        // the ledger, so they are skipped without an upload.
        var watermark = since
        var stalled = false

        suspend fun flushLedger() {
            if (unsaved.isEmpty()) return
            settings.markBackedUp(unsaved)
            unsaved.clear()
        }

        try {
            val projection = arrayOf(
                MediaStore.Images.Media._ID,
                MediaStore.Images.Media.DISPLAY_NAME,
                MediaStore.Images.Media.DATE_ADDED,
                MediaStore.Images.Media.SIZE,
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
                val sizeCol = cursor.getColumnIndexOrThrow(MediaStore.Images.Media.SIZE)
                val total = cursor.count
                var seen = 0

                while (cursor.moveToNext() && !isStopped) {
                    val id = cursor.getLong(idCol)
                    val displayName = cursor.getString(nameCol) ?: "$id.jpg"
                    val added = cursor.getLong(dateCol)
                    val size = cursor.getLong(sizeCol)
                    seen += 1

                    if (seen % PROGRESS_EVERY == 0) {
                        runCatching { setForeground(foregroundInfo(seen, total)) }
                    }

                    // The whole photo is held in memory to be hashed, so a
                    // pathologically large one is left for a desktop client
                    // rather than risking an OutOfMemoryError.
                    if (size > MAX_PHOTO_BYTES) {
                        // Permanently too big for this client to hash in
                        // memory, so retrying it for ever would only hold the
                        // watermark back and re-scan everything after it. Let
                        // it through and leave that file to a desktop client.
                        Log.w(TAG, "skipping $displayName: ${size / (1024 * 1024)} MiB is too large")
                        oversize += 1
                        if (!stalled) watermark = added
                        continue
                    }

                    val uri = ContentUris.withAppendedId(
                        MediaStore.Images.Media.EXTERNAL_CONTENT_URI, id
                    )

                    // Anything at all going wrong with one photo must not
                    // abandon the rest of the run, and must not advance the
                    // watermark past it. OutOfMemoryError is an Error, not an
                    // Exception, so it needs catching explicitly.
                    val outcome = try {
                        val bytes = applicationContext.contentResolver
                            .openInputStream(uri)?.use { it.readBytes() }
                        if (bytes == null) {
                            Outcome.FAILED
                        } else {
                            val hash = QuarkdriveNative.contentHash(bytes)
                            if (known.contains(hash)) {
                                Outcome.ALREADY_PRESENT
                            } else {
                                api.upload(config.vault, remotePath(displayName, added), bytes)
                                known.add(hash)
                                unsaved.add(hash)
                                Outcome.UPLOADED
                            }
                        }
                    } catch (e: OutOfMemoryError) {
                        Log.w(TAG, "out of memory on $displayName", e)
                        Outcome.FAILED
                    } catch (e: Exception) {
                        Log.w(TAG, "backup failed for $displayName", e)
                        Outcome.FAILED
                    }

                    when (outcome) {
                        Outcome.UPLOADED -> {
                            uploadedCount += 1
                            // Persist as we go: a worker stopped at the
                            // execution limit keeps everything done so far.
                            if (unsaved.size >= LEDGER_BATCH) {
                                flushLedger()
                                if (!stalled) settings.recordBackupTime(watermark)
                            }
                        }
                        Outcome.ALREADY_PRESENT -> skipped += 1
                        Outcome.FAILED -> {
                            failed += 1
                            stalled = true
                        }
                    }

                    if (outcome != Outcome.FAILED && !stalled) watermark = added
                }
            }

            flushLedger()
            settings.recordBackupTime(watermark)

            Log.i(
                TAG,
                "backup finished: $uploadedCount uploaded, $skipped already present, " +
                    "$failed failed, $oversize too large",
            )
            if (failed > 0 && runAttemptCount < MAX_ATTEMPTS) {
                // Progress is saved; the retry picks up from the watermark.
                Result.retry()
            } else {
                Result.success(
                    workDataOf(
                        KEY_UPLOADED to uploadedCount,
                        KEY_SKIPPED to skipped,
                        KEY_FAILED to failed,
                        KEY_OVERSIZE to oversize,
                    )
                )
            }
        } catch (e: Exception) {
            Log.w(TAG, "backup run failed", e)
            runCatching { flushLedger() }
            runCatching { if (!stalled) settings.recordBackupTime(watermark) }
            if (runAttemptCount < MAX_ATTEMPTS) Result.retry() else Result.failure()
        }
    }

    private enum class Outcome { UPLOADED, ALREADY_PRESENT, FAILED }

    /**
     * Where the photo lands in the vault.
     *
     * Filed under the date it was taken, because camera counters wrap and
     * reset: two distinct photos are quite often both called `IMG_0001.jpg`,
     * and a flat `Camera/` folder would let the second silently replace the
     * first.
     */
    private fun remotePath(displayName: String, addedEpochSeconds: Long): String {
        val stamp = Date(TimeUnit.SECONDS.toMillis(addedEpochSeconds))
        val folder = SimpleDateFormat("yyyy/MM", Locale.US).format(stamp)
        return "Camera/$folder/${sanitise(displayName)}"
    }

    private fun sanitise(name: String): String =
        name.replace(Regex("[/\\\\:*?\"<>|]"), "_")
            .trim('.', ' ')
            .ifBlank { "photo.jpg" }

    companion object {
        private const val TAG = "BackupWorker"
        private const val MAX_ATTEMPTS = 3
        private const val LEDGER_BATCH = 10
        private const val PROGRESS_EVERY = 5
        private const val NOTIFICATION_ID = 4711
        private const val CHANNEL_ID = "quarkdrive-backup"
        private const val MAX_PHOTO_BYTES = 256L * 1024 * 1024
        const val KEY_UPLOADED = "uploaded"
        const val KEY_SKIPPED = "skipped"
        const val KEY_FAILED = "failed"
        const val KEY_OVERSIZE = "oversize"
        const val WORK_NAME = "quarkdrive-camera-backup"
    }
}

/** Enqueues and cancels the periodic backup job. */
object BackupScheduler {

    /** 15 minutes is the shortest interval WorkManager allows. */
    private const val INTERVAL_MINUTES = 15L

    fun enable(context: Context) {
        val constraints = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED)
            .setRequiresBatteryNotLow(true)
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

    /**
     * Make sure the periodic job exists whenever backup is switched on.
     *
     * Called at sign-in and on every app start: enabling the setting alone
     * used to leave the job unscheduled, so a fresh install backed nothing up
     * until the user toggled the switch by hand.
     */
    fun sync(context: Context, autoBackup: Boolean) {
        if (autoBackup) enable(context) else disable(context)
    }

    /** Upload anything new right now, rather than waiting for the next period. */
    fun runOnce(context: Context) {
        val request = OneTimeWorkRequestBuilder<BackupWorker>()
            .setConstraints(
                Constraints.Builder()
                    .setRequiredNetworkType(NetworkType.CONNECTED)
                    .build()
            )
            .build()
        WorkManager.getInstance(context).enqueue(request)
    }
}
