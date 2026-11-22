plugins {
    `kotlin-dsl`
}

gradlePlugin {
    plugins {
        register("androidApplication") {
            id = "demo.android.application"
        }
    }
}
