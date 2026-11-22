plugins {
    id("com.android.application")
}

android {
    defaultConfig {
        targetSdk = computeTargetSdk()
    }
    productFlavors {
        create("legacy") {
            targetSdk = 30 + 4
        }
        create("env") {
            targetSdk = System.getenv("TARGET_SDK")?.toInt() ?: 30
        }
        create("undefined") {
            targetSdk = someUndefinedName
        }
    }
}
