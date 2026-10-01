#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

static int result = 1;
static gboolean evaluating = FALSE;
static void evaluated(GObject *source, GAsyncResult *async_result, gpointer unused) {
    (void)unused;
    GError *error = NULL;
    JSCValue *value = webkit_web_view_evaluate_javascript_finish(WEBKIT_WEB_VIEW(source), async_result, &error);
    evaluating = FALSE;
    if (error) {
        g_printerr("JavaScript probe failed: %s\n", error->message);
        g_error_free(error);
        gtk_main_quit();
        return;
    }
    if (value && jsc_value_to_boolean(value)) {
        g_print("{\"version\":[%u,%u,%u],\"javascript\":true,\"decodedVideo\":true}\n",
                webkit_get_major_version(), webkit_get_minor_version(), webkit_get_micro_version());
        result = 0;
        gtk_main_quit();
    }
    if (value) g_object_unref(value);
}
static gboolean poll_video(gpointer data) {
    if (!evaluating) {
        evaluating = TRUE;
        webkit_web_view_evaluate_javascript(WEBKIT_WEB_VIEW(data),
            "Boolean(window.brickProbe === 42 && document.querySelector('video')?.videoWidth === 64 && document.querySelector('video').currentTime > 0.15 && document.querySelector('video').readyState >= 2)",
            -1, NULL, NULL, NULL, evaluated, NULL);
    }
    return G_SOURCE_CONTINUE;
}
static gboolean expired(gpointer unused) {
    (void)unused;
    g_printerr("Runtime JavaScript/video probe timed out.\n");
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}
int main(int argc, char **argv) {
    if (argc != 2 || !g_str_has_prefix(argv[1], "http://127.0.0.1:")) return 2;
    gtk_init(&argc, &argv);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    GtkWidget *view = webkit_web_view_new();
    webkit_settings_set_media_playback_requires_user_gesture(webkit_web_view_get_settings(WEBKIT_WEB_VIEW(view)), FALSE);
    gtk_container_add(GTK_CONTAINER(window), view);
    gtk_window_set_default_size(GTK_WINDOW(window), 320, 240);
    gtk_widget_show_all(window);
    webkit_web_view_load_uri(WEBKIT_WEB_VIEW(view), argv[1]);
    guint poll = g_timeout_add(500, poll_video, view);
    g_timeout_add_seconds(30, expired, NULL);
    gtk_main();
    g_source_remove(poll);
    gtk_widget_destroy(window);
    return result;
}
