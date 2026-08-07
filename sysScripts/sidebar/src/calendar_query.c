#include <glib.h>
#include <libecal/libecal.h>
#include <libedataserver/libedataserver.h>
#include <libical-glib/libical-glib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    gchar *uid;
    gchar *summary;
    gchar *start_date;
    gchar *end_date;
    gchar *display_time;
    gint64 duration_minutes;
    gboolean all_day;
    gint64 sort_key;
} CalendarRow;

enum {
    CONNECT_TIMEOUT_SECONDS = 2,
};

typedef struct {
    const gchar *sexp;
    GPtrArray *rows;
    GMutex *rows_mutex;
} QueryContext;

typedef struct {
    ESource *source;
    QueryContext *context;
} SourceQueryTask;

static gboolean parse_date_arg(const char *value, gint *year, gint *month, gint *day) {
    if (!value || strlen(value) != 10) {
        return FALSE;
    }
    return sscanf(value, "%4d-%2d-%2d", year, month, day) == 3;
}

static gchar *make_utc_range_string(gint year, gint month, gint day, gboolean end_of_day) {
    struct tm local_tm = {0};
    local_tm.tm_year = year - 1900;
    local_tm.tm_mon = month - 1;
    local_tm.tm_mday = day;
    local_tm.tm_hour = 0;
    local_tm.tm_min = 0;
    local_tm.tm_sec = 0;

    time_t local_epoch = mktime(&local_tm);
    struct tm *utc = gmtime(&local_epoch);
    if (!utc) {
        return NULL;
    }

    if (end_of_day) {
        local_epoch += 24 * 60 * 60;
        utc = gmtime(&local_epoch);
        if (!utc) {
            return NULL;
        }
    }

    return g_strdup_printf("%04d%02d%02dT%02d%02d%02dZ",
                           utc->tm_year + 1900,
                           utc->tm_mon + 1,
                           utc->tm_mday,
                           utc->tm_hour,
                           utc->tm_min,
                           utc->tm_sec);
}

static gchar *escape_json(const gchar *value) {
    GString *out = g_string_new(NULL);
    for (const unsigned char *p = (const unsigned char *)value; p && *p; ++p) {
        switch (*p) {
            case '\\': g_string_append(out, "\\\\"); break;
            case '"': g_string_append(out, "\\\""); break;
            case '\b': g_string_append(out, "\\b"); break;
            case '\f': g_string_append(out, "\\f"); break;
            case '\n': g_string_append(out, "\\n"); break;
            case '\r': g_string_append(out, "\\r"); break;
            case '\t': g_string_append(out, "\\t"); break;
            default:
                if (*p < 0x20) {
                    g_string_append_printf(out, "\\u%04x", *p);
                } else {
                    g_string_append_c(out, (gchar)*p);
                }
        }
    }
    return g_string_free(out, FALSE);
}

static gint compare_rows(gconstpointer a, gconstpointer b, gpointer user_data) {
    (void)user_data;
    const CalendarRow *left = a;
    const CalendarRow *right = b;

    if (left->sort_key < right->sort_key) {
        return -1;
    }
    if (left->sort_key > right->sort_key) {
        return 1;
    }

    gint summary_cmp = g_ascii_strcasecmp(left->summary ? left->summary : "", right->summary ? right->summary : "");
    if (summary_cmp != 0) {
        return summary_cmp;
    }

    return g_strcmp0(left->uid ? left->uid : "", right->uid ? right->uid : "");
}

static time_t ic_time_to_time_t(const ICalTime *value) {
    ICalTimezone *timezone = i_cal_time_get_timezone(value);
    return i_cal_time_as_timet_with_zone(value, timezone);
}

static gchar *text_or_empty(ECalComponentText *text) {
    if (!text) {
        return g_strdup("");
    }
    const gchar *value = e_cal_component_text_get_value(text);
    return g_strdup(value ? value : "");
}

static gboolean build_row_from_component(ECalComponent *component, CalendarRow *row) {
    memset(row, 0, sizeof(*row));

    row->uid = g_strdup(e_cal_component_get_uid(component));

    ECalComponentText *summary = e_cal_component_get_summary(component);
    row->summary = text_or_empty(summary);

    ECalComponentDateTime *dtstart = e_cal_component_get_dtstart(component);
    ICalTime *start_value = dtstart ? e_cal_component_datetime_get_value(dtstart) : NULL;
    if (!start_value) {
        return FALSE;
    }

    gboolean all_day = i_cal_time_is_date(start_value);
    row->all_day = all_day;

    if (all_day) {
        gint start_year = i_cal_time_get_year(start_value);
        gint start_month = i_cal_time_get_month(start_value);
        gint start_day = i_cal_time_get_day(start_value);
        row->start_date = g_strdup_printf("%04d-%02d-%02d", start_year, start_month, start_day);

        ECalComponentDateTime *dtend = e_cal_component_get_dtend(component);
        gint end_year = start_year;
        gint end_month = start_month;
        gint end_day = start_day + 1;
        ICalTime *end_value = dtend ? e_cal_component_datetime_get_value(dtend) : NULL;
        if (end_value && i_cal_time_is_date(end_value)) {
            end_year = i_cal_time_get_year(end_value);
            end_month = i_cal_time_get_month(end_value);
            end_day = i_cal_time_get_day(end_value);
        }
        row->end_date = g_strdup_printf("%04d-%02d-%02d", end_year, end_month, end_day);
        row->display_time = g_strdup("All day");
        GDate start_gdate = {0};
        GDate end_gdate = {0};
        g_date_set_dmy(&start_gdate, start_day, start_month, start_year);
        g_date_set_dmy(&end_gdate, end_day, end_month, end_year);
        row->duration_minutes = (gint64)g_date_days_between(&start_gdate, &end_gdate) * 1440;
    } else {
        time_t start_ts = ic_time_to_time_t(start_value);

        ECalComponentDateTime *dtend = e_cal_component_get_dtend(component);
        ICalTime *end_value = dtend ? e_cal_component_datetime_get_value(dtend) : NULL;
        if (end_value) {
            time_t end_ts = ic_time_to_time_t(end_value);
            GDateTime *start_dt = g_date_time_new_from_unix_local((gint64)start_ts);
            GDateTime *end_dt = g_date_time_new_from_unix_local((gint64)end_ts);
            if (!start_dt || !end_dt) {
                if (start_dt) g_date_time_unref(start_dt);
                if (end_dt) g_date_time_unref(end_dt);
                return FALSE;
            }

            row->start_date = g_strdup_printf("%04d-%02d-%02d",
                                              g_date_time_get_year(start_dt),
                                              g_date_time_get_month(start_dt),
                                              g_date_time_get_day_of_month(start_dt));
            row->end_date = g_strdup_printf("%04d-%02d-%02d",
                                            g_date_time_get_year(end_dt),
                                            g_date_time_get_month(end_dt),
                                            g_date_time_get_day_of_month(end_dt));
            row->display_time = g_date_time_format(start_dt, "%H:%M");
            row->duration_minutes = (gint64)((g_date_time_to_unix(end_dt) - g_date_time_to_unix(start_dt)) / 60);
            if (row->duration_minutes < 0) {
                row->duration_minutes = 0;
            }
            row->sort_key = (gint64)g_date_time_to_unix(start_dt);
            g_date_time_unref(start_dt);
            g_date_time_unref(end_dt);
        } else {
            GDateTime *start_dt = g_date_time_new_from_unix_local((gint64)start_ts);
            if (!start_dt) {
                return FALSE;
            }

            row->start_date = g_strdup_printf("%04d-%02d-%02d",
                                              g_date_time_get_year(start_dt),
                                              g_date_time_get_month(start_dt),
                                              g_date_time_get_day_of_month(start_dt));
            row->end_date = g_strdup(row->start_date);
            row->display_time = g_date_time_format(start_dt, "%H:%M");
            row->duration_minutes = 0;
            row->sort_key = (gint64)g_date_time_to_unix(start_dt);
            g_date_time_unref(start_dt);
        }
    }

    if (row->all_day) {
        GDateTime *sort_dt = g_date_time_new_local(
            i_cal_time_get_year(start_value),
            i_cal_time_get_month(start_value),
            i_cal_time_get_day(start_value),
            0, 0, 0.0);
        if (sort_dt) {
            row->sort_key = (gint64)g_date_time_to_unix(sort_dt);
            g_date_time_unref(sort_dt);
        }
    }

    return TRUE;
}

static void free_row(CalendarRow *row) {
    g_free(row->uid);
    g_free(row->summary);
    g_free(row->start_date);
    g_free(row->end_date);
    g_free(row->display_time);
    g_free(row);
}

static void print_row_json(const CalendarRow *row, gboolean *first) {
    gchar *uid = escape_json(row->uid ? row->uid : "");
    gchar *summary = escape_json(row->summary ? row->summary : "");
    gchar *start_date = escape_json(row->start_date ? row->start_date : "");
    gchar *end_date = escape_json(row->end_date ? row->end_date : "");
    gchar *display_time = escape_json(row->display_time ? row->display_time : "");

    if (!*first) {
        fputc(',', stdout);
    }
    *first = FALSE;

    fprintf(stdout,
            "{\"uid\":\"%s\",\"summary\":\"%s\",\"start_date\":\"%s\",\"end_date\":\"%s\",\"display_time\":\"%s\",\"duration_minutes\":%lld,\"all_day\":%s,\"sort_key\":%lld}",
            uid,
            summary,
            start_date,
            end_date,
            display_time,
            (long long)row->duration_minutes,
            row->all_day ? "true" : "false",
            (long long)row->sort_key);

    g_free(uid);
    g_free(summary);
    g_free(start_date);
    g_free(end_date);
    g_free(display_time);
}

static void query_source_worker(gpointer data, gpointer user_data) {
    SourceQueryTask *task = data;
    QueryContext *context = user_data;
    GError *error = NULL;

    EClient *client = e_cal_client_connect_sync(
        task->source,
        E_CAL_CLIENT_SOURCE_TYPE_EVENTS,
        CONNECT_TIMEOUT_SECONDS,
        NULL,
        &error);
    if (!client) {
        if (error) {
            g_error_free(error);
        }
        g_object_unref(task->source);
        g_free(task);
        return;
    }

    GSList *components = NULL;
    gboolean ok = e_cal_client_get_object_list_as_comps_sync(
        E_CAL_CLIENT(client),
        context->sexp,
        &components,
        NULL,
        &error);
    if (ok) {
        GPtrArray *local_rows = g_ptr_array_new();

        for (GSList *item = components; item != NULL; item = item->next) {
            ECalComponent *component = item->data;
            CalendarRow *row = g_new0(CalendarRow, 1);
            if (build_row_from_component(component, row)) {
                g_ptr_array_add(local_rows, row);
            } else {
                free_row(row);
            }
            g_object_unref(component);
        }

        g_mutex_lock(context->rows_mutex);
        for (guint i = 0; i < local_rows->len; ++i) {
            g_ptr_array_add(context->rows, g_ptr_array_index(local_rows, i));
        }
        g_mutex_unlock(context->rows_mutex);

        g_ptr_array_set_free_func(local_rows, NULL);
        g_ptr_array_free(local_rows, TRUE);
    } else if (error) {
        g_error_free(error);
    }

    g_slist_free(components);
    g_object_unref(client);
    g_object_unref(task->source);
    g_free(task);
}

int main(int argc, char **argv) {
    if (argc != 3) {
        puts("[]");
        return 0;
    }

    gint year = 0, month = 0, day = 0;
    gint end_year = 0, end_month = 0, end_day = 0;
    if (!parse_date_arg(argv[1], &year, &month, &day) || !parse_date_arg(argv[2], &end_year, &end_month, &end_day)) {
        puts("[]");
        return 0;
    }

    gchar *start_utc = make_utc_range_string(year, month, day, FALSE);
    gchar *end_utc = make_utc_range_string(end_year, end_month, end_day, FALSE);
    if (!start_utc || !end_utc) {
        g_free(start_utc);
        g_free(end_utc);
        puts("[]");
        return 0;
    }

    gchar *sexp = g_strdup_printf("(occur-in-time-range? (make-time \"%s\") (make-time \"%s\"))", start_utc, end_utc);
    g_free(start_utc);
    g_free(end_utc);

    ESourceRegistry *registry = e_source_registry_new_sync(NULL, NULL);
    if (!registry) {
        g_free(sexp);
        puts("[]");
        return 0;
    }

    GList *sources = e_source_registry_list_enabled(registry, E_SOURCE_EXTENSION_CALENDAR);
    GPtrArray *rows = g_ptr_array_new_with_free_func((GDestroyNotify)free_row);
    GMutex rows_mutex;
    g_mutex_init(&rows_mutex);

    QueryContext context = {
        .sexp = sexp,
        .rows = rows,
        .rows_mutex = &rows_mutex,
    };

    GError *pool_error = NULL;
    gint max_threads = g_list_length(sources);
    if (max_threads < 1) {
        max_threads = 1;
    }

    GThreadPool *pool = g_thread_pool_new(query_source_worker, &context, max_threads, FALSE, &pool_error);
    if (!pool) {
        if (pool_error) {
            g_error_free(pool_error);
        }
        g_list_free_full(sources, g_object_unref);
        g_free(sexp);
        g_ptr_array_free(rows, TRUE);
        g_object_unref(registry);
        puts("[]");
        return 0;
    }

    for (GList *node = sources; node != NULL; node = node->next) {
        ESource *source = node->data;
        SourceQueryTask *task = g_new0(SourceQueryTask, 1);
        task->source = g_object_ref(source);
        task->context = &context;
        g_thread_pool_push(pool, task, NULL);
    }

    g_thread_pool_free(pool, FALSE, TRUE);

    g_list_free_full(sources, g_object_unref);
    g_free(sexp);
    g_mutex_clear(&rows_mutex);

    g_ptr_array_sort_with_data(rows, compare_rows, NULL);

    fputc('[', stdout);
    gboolean first = TRUE;
    for (guint i = 0; i < rows->len; ++i) {
        print_row_json(g_ptr_array_index(rows, i), &first);
    }
    fputc(']', stdout);
    fputc('\n', stdout);

    g_ptr_array_free(rows, TRUE);
    g_object_unref(registry);
    return 0;
}