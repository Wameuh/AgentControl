## Default Permission

Plugin appareil : `set_system_bars` ajuste l'apparence des barres
d'état/navigation Android suivant le thème de l'UI (icônes sombres en
thème clair, claires en thème sombre — Y4). `signingInfo` reste
Rust-only (verdict update in-place, pas de commande webview).

#### This default permission set includes the following:

- `allow-set-system-bars`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`ac-device:allow-set-system-bars`

</td>
<td>

Enables the set_system_bars command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`ac-device:deny-set-system-bars`

</td>
<td>

Denies the set_system_bars command without any pre-configured scope.

</td>
</tr>
</table>
