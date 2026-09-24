// === Group Mapping Rules ===
import { adminJson, adminRequest } from './core';

export interface MappingRule {
  id: number;
  provider_id: number | null;
  priority: number;
  match_type: string;
  match_field: string;
  match_value: string;
  group_id: number;
  created_at: string;
}

interface CreateMappingRuleRequest {
  provider_id?: number | null;
  priority?: number;
  match_type: string;
  match_field?: string;
  match_value: string;
  group_id: number;
}

interface UpdateMappingRuleRequest {
  provider_id?: number | null;
  priority?: number;
  match_type?: string;
  match_field?: string;
  match_value?: string;
  group_id?: number;
}

export async function getMappingRules(): Promise<MappingRule[]> {
  return adminJson('/api/admin/ext-auth/mappings', { context: 'Load group mappings' });
}

export async function createMappingRule(req: CreateMappingRuleRequest): Promise<MappingRule> {
  return adminJson('/api/admin/ext-auth/mappings', {
    method: 'POST',
    body: req,
    context: 'Create group mapping',
  });
}

export async function updateMappingRule(id: number, req: UpdateMappingRuleRequest): Promise<MappingRule> {
  return adminJson(`/api/admin/ext-auth/mappings/${id}`, {
    method: 'PUT',
    body: req,
    context: 'Update group mapping',
  });
}

export async function deleteMappingRule(id: number): Promise<void> {
  await adminRequest(`/api/admin/ext-auth/mappings/${id}`, {
    method: 'DELETE',
    context: 'Delete group mapping',
  });
}

interface MappingPreviewResponse {
  group_ids: number[];
  group_names: string[];
}

export async function previewMapping(email: string): Promise<MappingPreviewResponse> {
  return adminJson('/api/admin/ext-auth/mappings/preview', {
    method: 'POST',
    body: { email },
    context: 'Mapping preview',
  });
}
