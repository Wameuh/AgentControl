import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

// JitPack pour l'AAR sherpa-onnx — le projet racine n'a pas de
// repositoriesMode restrictif, les repos du module restent autorisés.
repositories {
    maven { url = uri("https://jitpack.io") }
}

android {
    namespace = "dev.agentcontrol.stt"
    compileSdk = 36

    defaultConfig {
        minSdk = 24
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

dependencies {
    implementation(project(":tauri-android"))
    // Moteur ASR on-device (jniLibs arm64 packagés avec l'APK).
    implementation("com.github.k2-fsa.sherpa-onnx:sherpa-onnx:v1.13.5")
}
