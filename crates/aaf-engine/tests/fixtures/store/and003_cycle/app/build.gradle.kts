plugins {
    id("com.android.application")
}

val first = second
val second = first

android {
    defaultConfig {
        targetSdk = first
    }
}
