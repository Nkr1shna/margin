// Margin's source pickers on Android (Storage Access Framework). build.rs copies this
// file next to wry's Kotlin files in the Gradle project dx generates; Rust calls it
// through JNI (src/picker/android.rs).

package {{package}}

import android.app.Activity
import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import java.io.File
import java.util.concurrent.atomic.AtomicInteger

/**
 * One pick: registers a one-off activity result callback, launches the system picker and
 * hands what was picked to Rust through [onPicked] (registered from Rust with
 * RegisterNatives).
 */
class MarginPicker private constructor(private val token: Long) {
    /** `uris`: the picked documents (files) or tree (folder), empty when cancelled, null
     * when the picker couldn't be shown. */
    private external fun onPicked(token: Long, uris: Array<String>?)

    private fun launch(activity: Activity, folder: Boolean) {
        val registry = (activity as? ComponentActivity)?.activityResultRegistry
        if (registry == null) {
            onPicked(token, null)
            return
        }
        val key = "margin-picker-" + next.incrementAndGet()
        try {
            if (folder) {
                lateinit var launcher: ActivityResultLauncher<Uri?>
                launcher = registry.register(key, ActivityResultContracts.OpenDocumentTree()) { uri ->
                    launcher.unregister()
                    onPicked(token, if (uri == null) emptyArray() else arrayOf(uri.toString()))
                }
                launcher.launch(null)
            } else {
                lateinit var launcher: ActivityResultLauncher<Array<String>>
                launcher = registry.register(key, ActivityResultContracts.OpenMultipleDocuments()) { uris ->
                    launcher.unregister()
                    onPicked(token, uris.map { it.toString() }.toTypedArray())
                }
                launcher.launch(arrayOf("*/*"))
            }
        } catch (e: Exception) {
            Log.e(TAG, "couldn't show the picker", e)
            onPicked(token, null)
        }
    }

    companion object {
        private const val TAG = "MarginPicker"
        private val next = AtomicInteger()

        /** Show the files picker (several at once) or the folder picker. */
        @JvmStatic
        fun pick(activity: Activity, folder: Boolean, token: Long) {
            val picker = MarginPicker(token)
            activity.runOnUiThread { picker.launch(activity, folder) }
        }

        /** A picked document's display name and size ("" when unknown). */
        @JvmStatic
        fun describe(context: Context, uri: String): Array<String> {
            val parsed = Uri.parse(uri)
            var name = (parsed.lastPathSegment ?: "").substringAfterLast('/')
            var size = ""
            try {
                val columns = arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
                context.contentResolver.query(parsed, columns, null, null, null)?.use { c ->
                    if (c.moveToFirst()) {
                        if (!c.isNull(0)) name = c.getString(0)
                        if (!c.isNull(1)) size = c.getLong(1).toString()
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "couldn't describe $uri: $e")
            }
            return arrayOf(name, size)
        }

        /** A picked tree's root folder: its document id and display name. */
        @JvmStatic
        fun treeRoot(context: Context, tree: String): Array<String>? {
            return try {
                val treeUri = Uri.parse(tree)
                val id = DocumentsContract.getTreeDocumentId(treeUri)
                val root = DocumentsContract.buildDocumentUriUsingTree(treeUri, id)
                var name = id.substringAfterLast(':').substringAfterLast('/')
                val columns = arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                context.contentResolver.query(root, columns, null, null, null)?.use { c ->
                    if (c.moveToFirst() && !c.isNull(0)) name = c.getString(0)
                }
                arrayOf(id, name)
            } catch (e: Exception) {
                Log.w(TAG, "couldn't open $tree: $e")
                null
            }
        }

        /**
         * The documents in a folder of a picked tree, five strings each: document id,
         * document URI, display name, MIME type and size ("" when unknown); null when the
         * folder can't be listed.
         */
        @JvmStatic
        fun children(context: Context, tree: String, parent: String): Array<String>? {
            return try {
                val treeUri = Uri.parse(tree)
                val uri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, parent)
                val columns = arrayOf(
                    DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                    DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                    DocumentsContract.Document.COLUMN_MIME_TYPE,
                    DocumentsContract.Document.COLUMN_SIZE,
                )
                val out = ArrayList<String>()
                context.contentResolver.query(uri, columns, null, null, null)?.use { c ->
                    while (c.moveToNext()) {
                        if (c.isNull(0)) {
                            continue
                        }
                        val id = c.getString(0)
                        out.add(id)
                        out.add(DocumentsContract.buildDocumentUriUsingTree(treeUri, id).toString())
                        out.add(if (c.isNull(1)) "" else c.getString(1))
                        out.add(if (c.isNull(2)) "" else c.getString(2))
                        out.add(if (c.isNull(3)) "" else c.getLong(3).toString())
                    }
                } ?: return null
                out.toTypedArray()
            } catch (e: Exception) {
                Log.w(TAG, "couldn't list $parent: $e")
                null
            }
        }

        /** Copy a document into a file; null when it's copied, else what went wrong. */
        @JvmStatic
        fun copy(context: Context, uri: String, path: String): String? {
            return try {
                val input = context.contentResolver.openInputStream(Uri.parse(uri))
                    ?: return "The document couldn't be opened."
                input.use { from -> File(path).outputStream().use { to -> from.copyTo(to) } }
                null
            } catch (e: Exception) {
                e.message ?: e.toString()
            }
        }
    }
}
