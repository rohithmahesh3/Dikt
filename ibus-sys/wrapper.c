#include "wrapper.h"
#include <ibus.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#ifndef DIKT_VERSION
#define DIKT_VERSION "unknown"
#endif

#define IBUS_BUS_NAME_REQUESTED_PRIMARY 1
#define IBUS_BUS_NAME_REQUESTED_REPLACED 2

static void *global_context = NULL;
static ibus_dikt_callback_key_event global_key_event_cb = NULL;
static ibus_dikt_callback_focus_in global_focus_in_cb = NULL;
static ibus_dikt_callback_focus_out global_focus_out_cb = NULL;
static ibus_dikt_callback_reset global_reset_cb = NULL;
static ibus_dikt_callback_enable global_enable_cb = NULL;
static ibus_dikt_callback_disable global_disable_cb = NULL;
static IBusBus *global_bus = NULL;
static IBusFactory *global_factory = NULL;

#define IBUS_TYPE_DIKT_ENGINE (ibus_dikt_engine_get_type())

GType ibus_dikt_engine_get_type(void);

typedef struct {
  IBusEngineClass parent;
} IBusDiktEngineClass;

static void ibus_dikt_engine_class_init(IBusDiktEngineClass *klass);
static void ibus_dikt_engine_init(IBusDiktEngine *engine);
static void ibus_dikt_engine_destroy(IBusDiktEngine *engine);

static gboolean ibus_dikt_engine_process_key_event(IBusEngine *engine,
                                                    guint keyval, guint keycode,
                                                    guint modifiers);

static void ibus_dikt_engine_focus_in(IBusEngine *engine);
static void ibus_dikt_engine_focus_out(IBusEngine *engine);
static void ibus_dikt_engine_reset(IBusEngine *engine);
static void ibus_dikt_engine_enable(IBusEngine *engine);
static void ibus_dikt_engine_disable(IBusEngine *engine);

G_DEFINE_TYPE(IBusDiktEngine, ibus_dikt_engine, IBUS_TYPE_ENGINE)

static void ibus_dikt_engine_class_init(IBusDiktEngineClass *klass) {
  IBusObjectClass *ibus_object_class = IBUS_OBJECT_CLASS(klass);
  IBusEngineClass *engine_class = IBUS_ENGINE_CLASS(klass);

  ibus_object_class->destroy = (IBusObjectDestroyFunc)ibus_dikt_engine_destroy;

  engine_class->process_key_event = ibus_dikt_engine_process_key_event;
  engine_class->focus_in = ibus_dikt_engine_focus_in;
  engine_class->focus_out = ibus_dikt_engine_focus_out;
  engine_class->reset = ibus_dikt_engine_reset;
  engine_class->enable = ibus_dikt_engine_enable;
  engine_class->disable = ibus_dikt_engine_disable;
}

static void ibus_dikt_engine_init(IBusDiktEngine *engine) { (void)engine; }

static void ibus_dikt_engine_destroy(IBusDiktEngine *engine) {
  ((IBusObjectClass *)ibus_dikt_engine_parent_class)
      ->destroy((IBusObject *)engine);
}

static gboolean ibus_dikt_engine_process_key_event(IBusEngine *engine,
                                                    guint keyval, guint keycode,
                                                    guint modifiers) {
  if (global_key_event_cb && global_context) {
    return global_key_event_cb(global_context, engine, keyval, keycode,
                               modifiers);
  }
  return FALSE;
}

static void ibus_dikt_engine_focus_in(IBusEngine *engine) {
  if (global_focus_in_cb && global_context) {
    global_focus_in_cb(global_context, engine);
  }
}

static void ibus_dikt_engine_focus_out(IBusEngine *engine) {
  if (global_focus_out_cb && global_context) {
    global_focus_out_cb(global_context, engine);
  }
}

static void ibus_dikt_engine_reset(IBusEngine *engine) {
  if (global_reset_cb && global_context) {
    global_reset_cb(global_context, engine);
  }
}

static void ibus_dikt_engine_enable(IBusEngine *engine) {
  if (global_enable_cb && global_context) {
    global_enable_cb(global_context, engine);
  }
}

static void ibus_dikt_engine_disable(IBusEngine *engine) {
  if (global_disable_cb && global_context) {
    global_disable_cb(global_context, engine);
  }
}

static void ibus_disconnected_cb(IBusBus *bus, gpointer user_data) {
  (void)bus;
  (void)user_data;
  ibus_quit();
}

void ibus_dikt_set_callback(void *ctx,
                             ibus_dikt_callback_key_event key_event_cb,
                             ibus_dikt_callback_focus_in focus_in_cb,
                             ibus_dikt_callback_focus_out focus_out_cb,
                             ibus_dikt_callback_reset reset_cb,
                             ibus_dikt_callback_enable enable_cb,
                             ibus_dikt_callback_disable disable_cb) {
  global_context = ctx;
  global_key_event_cb = key_event_cb;
  global_focus_in_cb = focus_in_cb;
  global_focus_out_cb = focus_out_cb;
  global_reset_cb = reset_cb;
  global_enable_cb = enable_cb;
  global_disable_cb = disable_cb;
}

int ibus_dikt_init(bool ibus_mode) {
  ibus_init();

  IBusBus *bus = ibus_bus_new();
  if (!bus) {
    fprintf(stderr, "Failed to create IBus bus\n");
    return 1;
  }

  if (!ibus_bus_is_connected(bus)) {
    fprintf(stderr, "IBus daemon not running\n");
    g_object_unref(bus);
    return 2;
  }

  GDBusConnection *conn = ibus_bus_get_connection(bus);
  if (!conn) {
    fprintf(stderr, "IBus bus has no connection\n");
    g_object_unref(bus);
    return 3;
  }

  IBusFactory *factory = ibus_factory_new(conn);
  if (!factory) {
    fprintf(stderr, "Failed to create IBus factory\n");
    g_object_unref(bus);
    return 4;
  }

  g_signal_connect(bus, "disconnected", G_CALLBACK(ibus_disconnected_cb), NULL);

  ibus_factory_add_engine(factory, "dikt", IBUS_TYPE_DIKT_ENGINE);

  global_bus = bus;
  global_factory = factory;

  if (ibus_mode) {
    guint result = ibus_bus_request_name(bus, "org.freedesktop.IBus.Dikt", 0);
    if (result != IBUS_BUS_NAME_REQUESTED_PRIMARY &&
        result != IBUS_BUS_NAME_REQUESTED_REPLACED) {
      fprintf(stderr, "Warning: Failed to acquire IBus name: %u\n", result);
    }
  } else {
    IBusComponent *component;

    component = ibus_component_new(
        "org.freedesktop.IBus.Dikt", "Dikt Speech-to-Text", DIKT_VERSION,
        "MIT", "Dikt Team", "https://github.com/rohithmahesh3/Dikt", "",
        "dikt-ibus");

    ibus_component_add_engine(
        component,
        ibus_engine_desc_new("dikt", "Dikt", "Dikt speech-to-text dictation",
                             "other", "MIT", "Dikt Team", "dikt", "default"));

    ibus_bus_register_component(bus, component);
    g_object_unref(component);
  }

  return 0;
}

void ibus_dikt_cleanup(void) {
  if (global_factory) {
    g_object_unref(global_factory);
    global_factory = NULL;
  }
  if (global_bus) {
    g_object_unref(global_bus);
    global_bus = NULL;
  }
}

gboolean ibus_dikt_set_global_engine(const gchar *engine_name) {
  if (!global_bus || !engine_name || !ibus_bus_is_connected(global_bus)) {
    return FALSE;
  }

  return ibus_bus_set_global_engine(global_bus, engine_name);
}

gchar *ibus_dikt_get_global_engine_name(void) {
  if (!global_bus || !ibus_bus_is_connected(global_bus)) {
    return NULL;
  }

  IBusEngineDesc *desc = ibus_bus_get_global_engine(global_bus);
  if (!desc) {
    return NULL;
  }

  const gchar *name = ibus_engine_desc_get_name(desc);
  gchar *result = name ? g_strdup(name) : NULL;
  g_object_unref(desc);
  return result;
}

static gsize daemon_ibus_initialized = 0;
static IBusBus *daemon_cached_bus = NULL;

/* Return a persistent, cached IBusBus for the daemon process.
 * Re-creates the connection only if it was never opened or has disconnected.
 * This avoids the ~50-200 ms overhead of ibus_bus_new() on every call,
 * which was causing engine-switch to lose the race against key events. */
static IBusBus *ibus_dikt_daemon_get_bus(void) {
  if (g_once_init_enter(&daemon_ibus_initialized)) {
    ibus_init();
    g_once_init_leave(&daemon_ibus_initialized, 1);
  }

  if (daemon_cached_bus && ibus_bus_is_connected(daemon_cached_bus)) {
    return daemon_cached_bus;
  }

  if (daemon_cached_bus) {
    g_object_unref(daemon_cached_bus);
    daemon_cached_bus = NULL;
  }

  daemon_cached_bus = ibus_bus_new();
  if (!daemon_cached_bus || !ibus_bus_is_connected(daemon_cached_bus)) {
    if (daemon_cached_bus) {
      g_object_unref(daemon_cached_bus);
      daemon_cached_bus = NULL;
    }
    return NULL;
  }

  return daemon_cached_bus;
}

gboolean ibus_dikt_daemon_set_global_engine(const gchar *engine_name) {
  if (!engine_name || strlen(engine_name) == 0) {
    return FALSE;
  }

  IBusBus *bus = ibus_dikt_daemon_get_bus();
  if (!bus) {
    return FALSE;
  }

  return ibus_bus_set_global_engine(bus, engine_name);
}

gchar *ibus_dikt_daemon_get_global_engine_name(void) {
  IBusBus *bus = ibus_dikt_daemon_get_bus();
  if (!bus) {
    return NULL;
  }

  IBusEngineDesc *desc = ibus_bus_get_global_engine(bus);
  if (!desc) {
    return NULL;
  }

  const gchar *name = ibus_engine_desc_get_name(desc);
  gchar *result = name ? g_strdup(name) : NULL;
  g_object_unref(desc);
  return result;
}
