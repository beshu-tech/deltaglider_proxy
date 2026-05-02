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
