import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

// JitPack pour l'AAR sherpa-onnx : la résolution du graphe de
// dépendances de l'app utilise les repositories de :app (allprojects
// du build racine généré par Tauri), pas ceux déclarés dans ce module
// — on l'ajoute donc à tous les projets depuis ici. À retirer si le
// build racine déclare un jour JitPack.
rootProject.allprojects {
    repositories {
        maven { url = uri("https://jitpack.io") }
    }
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
