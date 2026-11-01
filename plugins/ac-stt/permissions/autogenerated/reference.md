## Default Permission

Plugin STT : diagnostic `stt_ping`, gestion des modèles téléchargeables
(`stt_dl_*` — download à la demande depuis les settings, Android only),
cycle de dictée (`stt_state`/`stt_start`/`stt_stop`/`stt_set_language`),
choix du moteur (`stt_set_engine` — sherpa / SpeechRecognizer système)
et abonnement aux events stt_* (register/remove_listener — héritées de
la base Kotlin du plugin).

#### This default permission set includes the following:

- `allow-stt-ping`
- `allow-stt-dl-state`
- `allow-stt-dl-start`
- `allow-stt-dl-cancel`
- `allow-stt-dl-delete`
- `allow-stt-state`
- `allow-stt-start`
- `allow-stt-stop`
- `allow-stt-set-language`
- `allow-stt-set-engine`
- `allow-register-listener`
- `allow-remove-listener`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`ac-stt:allow-register-listener`

</td>
<td>

Enables the register_listener command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-register-listener`

</td>
<td>

Denies the register_listener command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-remove-listener`

</td>
<td>

Enables the remove_listener command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-remove-listener`

</td>
<td>

Denies the remove_listener command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-dl-cancel`

</td>
<td>

Enables the stt_dl_cancel command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-dl-cancel`

</td>
<td>

Denies the stt_dl_cancel command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-dl-delete`

</td>
<td>

Enables the stt_dl_delete command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-dl-delete`

</td>
<td>

Denies the stt_dl_delete command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-dl-start`

</td>
<td>

Enables the stt_dl_start command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-dl-start`

</td>
<td>

Denies the stt_dl_start command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-dl-state`

</td>
<td>

Enables the stt_dl_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-dl-state`

</td>
<td>

Denies the stt_dl_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-ping`

</td>
<td>

Enables the stt_ping command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-ping`

</td>
<td>

Denies the stt_ping command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-set-engine`

</td>
<td>

Enables the stt_set_engine command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-set-engine`

</td>
<td>

Denies the stt_set_engine command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-set-language`

</td>
<td>

Enables the stt_set_language command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-set-language`

</td>
<td>

Denies the stt_set_language command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-start`

</td>
<td>

Enables the stt_start command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-start`

</td>
<td>

Denies the stt_start command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-state`

</td>
<td>

Enables the stt_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-state`

</td>
<td>

Denies the stt_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:allow-stt-stop`

</td>
<td>

Enables the stt_stop command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-stt:deny-stt-stop`

</td>
<td>

Denies the stt_stop command without any pre-configured scope.

</td>
</tr>
</table>
