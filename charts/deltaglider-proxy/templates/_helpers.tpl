{{/*
Expand the name of the chart.
*/}}
{{- define "deltaglider-proxy.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "deltaglider-proxy.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "deltaglider-proxy.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels.
*/}}
{{- define "deltaglider-proxy.labels" -}}
helm.sh/chart: {{ include "deltaglider-proxy.chart" . }}
{{ include "deltaglider-proxy.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{/*
Selector labels.
*/}}
{{- define "deltaglider-proxy.selectorLabels" -}}
app.kubernetes.io/name: {{ include "deltaglider-proxy.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Create the service account name.
*/}}
{{- define "deltaglider-proxy.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "deltaglider-proxy.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "deltaglider-proxy.configName" -}}
{{- printf "%s-config" (include "deltaglider-proxy.fullname" .) -}}
{{- end -}}

{{- define "deltaglider-proxy.secretName" -}}
{{- default (printf "%s-secret" (include "deltaglider-proxy.fullname" .)) .Values.auth.existingSecret -}}
{{- end -}}

{{- define "deltaglider-proxy.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/*
The size limit of the /tmp emptyDir in bytes, from tmp.sizeLimit (Mi or Gi).
The spool (DGP_SPOOL_DIR, default /tmp/dgp-spool) lives in /tmp.
*/}}
{{- define "deltaglider-proxy.tmpSizeLimitBytes" -}}
{{- $s := toString .Values.tmp.sizeLimit -}}
{{- if hasSuffix "Gi" $s -}}
{{- mul (trimSuffix "Gi" $s | atoi) 1073741824 -}}
{{- else if hasSuffix "Mi" $s -}}
{{- mul (trimSuffix "Mi" $s | atoi) 1048576 -}}
{{- else -}}
{{- fail (printf "tmp.sizeLimit %q must be in Mi or Gi (for example 2Gi): the chart sets DGP_SPOOL_MAX_BYTES below it" $s) -}}
{{- end -}}
{{- end -}}

{{/*
DGP_SPOOL_MAX_BYTES: three quarters of tmp.sizeLimit, so the spool never
reaches the limit (a pod past its emptyDir limit is evicted).
*/}}
{{- define "deltaglider-proxy.spoolMaxBytes" -}}
{{- div (mul (include "deltaglider-proxy.tmpSizeLimitBytes" . | atoi) 3) 4 -}}
{{- end -}}
