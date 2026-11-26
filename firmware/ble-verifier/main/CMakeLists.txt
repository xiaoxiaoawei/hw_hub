idf_component_register(SRCS "app_main.c" "ble_protocol.c"
    INCLUDE_DIRS "." REQUIRES bt nvs_flash esp_driver_uart esp_driver_gpio esp_driver_ledc esp_driver_usb_serial_jtag json esp_timer esp_app_format)
